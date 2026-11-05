//! The Collection Schema: each declared field's Field Type and whether it
//! is a Strict Field (ADR 0011).
//!
//! Collections are schemaless, so the schema exists mainly to give
//! indexed fields their Field Types: an Index has no type of its own and
//! reads its field's from here (see [`crate::index`]). The schema alone
//! decides which writes a Strict Field rejects, whether or not the field is
//! indexed; an Index only stops its field's declaration from changing.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use bson::{Bson, RawDocument};
use psdb_storage::StorageEngine;

use crate::catalog::CollectionEntry;
use crate::collection_path::CollectionPath;
use crate::document::{self, lookup_raw};
use crate::error::{Error, Result};
use crate::field_path::FieldPath;
use crate::filter::TWO_63;
use crate::transaction::Transaction;

/// The kind of value a field holds, declared in the Collection Schema.
///
/// Collections stay schemaless: a Document whose field holds another type
/// is still stored, unless the field is strict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FieldType {
    /// 64-bit integers.
    Int,
    /// Doubles.
    Double,
    /// Strings.
    String,
    /// BSON dates.
    Date,
    /// Booleans.
    Bool,
    /// ObjectIds.
    ObjectId,
    /// Binary values.
    Binary,
}

impl FieldType {
    /// Every Field Type, in the order of [`FieldType::name`]'s list.
    pub const ALL: [FieldType; 7] = [
        FieldType::Int,
        FieldType::Double,
        FieldType::String,
        FieldType::Date,
        FieldType::Bool,
        FieldType::ObjectId,
        FieldType::Binary,
    ];

    /// The name used in the Catalog and by clients: `int`, `double`,
    /// `string`, `date`, `bool`, `objectId` or `binary`.
    pub fn name(self) -> &'static str {
        match self {
            FieldType::Int => "int",
            FieldType::Double => "double",
            FieldType::String => "string",
            FieldType::Date => "date",
            FieldType::Bool => "bool",
            FieldType::ObjectId => "objectId",
            FieldType::Binary => "binary",
        }
    }

    /// The Field Type of `value`, if it has one: the one place BSON types
    /// map to Field Types. Both integer widths are `int`.
    pub(crate) fn of(value: &Bson) -> Option<FieldType> {
        Some(match value {
            Bson::Int32(_) | Bson::Int64(_) => FieldType::Int,
            Bson::Double(_) => FieldType::Double,
            Bson::String(_) => FieldType::String,
            Bson::DateTime(_) => FieldType::Date,
            Bson::Boolean(_) => FieldType::Bool,
            Bson::ObjectId(_) => FieldType::ObjectId,
            Bson::Binary(_) => FieldType::Binary,
            _ => return None,
        })
    }

    /// Whether this is `int` or `double`, which filters compare with each
    /// other, as numbers.
    pub(crate) fn is_number(self) -> bool {
        matches!(self, FieldType::Int | FieldType::Double)
    }

    /// Whether filters can compare `value` with values of this type.
    pub(crate) fn compares_with(self, value: &Bson) -> bool {
        FieldType::of(value).is_some_and(|t| t == self || (t.is_number() && self.is_number()))
    }
}

impl fmt::Display for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for FieldType {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        FieldType::ALL
            .into_iter()
            .find(|t| t.name() == s)
            .ok_or_else(|| {
                Error::InvalidSchema(format!(
                    "unknown field type {s:?}; use int, double, string, date, bool, objectId or binary"
                ))
            })
    }
}

/// A field's declaration in a Collection Schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSchema {
    /// The kind of value the field holds.
    pub field_type: FieldType,
    /// Whether this is a Strict Field: writes giving it a value of another
    /// type fail with [`Error::TypeMismatch`]. Missing and null are always
    /// allowed.
    pub strict: bool,
}

/// A Collection Schema: Field path → its declaration.
pub(crate) type Schema = BTreeMap<FieldPath, FieldSchema>;

/// Fail with [`Error::TypeMismatch`] unless the value at `path` is missing,
/// null, or converts exactly to the field's type.
pub(crate) fn check_field(doc: &RawDocument, path: &FieldPath, field: FieldSchema) -> Result<()> {
    match lookup_raw(doc, path)? {
        None | Some(Bson::Null) => Ok(()),
        Some(v) if fits_exactly(&v, field.field_type) => Ok(()),
        Some(v) => Err(Error::TypeMismatch(format!(
            "{path} is a strict {} field, but this document has {v}",
            field.field_type
        ))),
    }
}

/// Whether `value` belongs in a Strict Field of type `ty` without losing
/// anything: an `int` accepts `30.0` (which JavaScript sends for `30`) but
/// not `30.5`; a `double` accepts integers up to 2⁵³ but not NaN.
fn fits_exactly(value: &Bson, ty: FieldType) -> bool {
    const TWO_53: u64 = 1 << 53;
    match (value, ty) {
        (Bson::Int32(_) | Bson::Int64(_), FieldType::Int) => true,
        (Bson::Double(f), FieldType::Int) => f.fract() == 0.0 && *f >= -TWO_63 && *f < TWO_63,
        (Bson::Int32(_), FieldType::Double) => true,
        (Bson::Int64(n), FieldType::Double) => n.unsigned_abs() <= TWO_53,
        (Bson::Double(f), FieldType::Double) => !f.is_nan(),
        _ => FieldType::of(value) == Some(ty),
    }
}

impl CollectionEntry {
    /// Fail with [`Error::TypeMismatch`] if `doc` breaks any Strict Field.
    pub(crate) fn check_strict(&self, doc: &bson::RawDocument) -> Result<()> {
        for (path, field) in self.schema.iter().filter(|(_, f)| f.strict) {
            check_field(doc, path, *field)?;
        }
        Ok(())
    }
}

impl<E: StorageEngine> Transaction<'_, E> {
    /// Declare a field's Field Type in the Collection Schema, or change its
    /// declaration.
    ///
    /// While an Index on the field exists, its type cannot change, and a
    /// field with a unique Index cannot stop being strict: drop the Index
    /// first. Making a field strict reads every Document and fails with
    /// [`Error::TypeMismatch`] if any breaks the rule.
    pub fn set_field_type(
        &mut self,
        collection: &CollectionPath,
        path: &FieldPath,
        field: FieldSchema,
    ) -> Result<()> {
        if path.is_under_id() {
            return Err(Error::InvalidSchema(
                "_id cannot be declared or indexed; Documents are already keyed by it".into(),
            ));
        }
        let mut entry = self.collection_entry(collection)?;
        let old = entry.schema.get(path).copied();
        if let (Some(ix), Some(old)) = (entry.index_on(path), old) {
            if old.field_type != field.field_type {
                return Err(Error::InvalidSchema(format!(
                    "{path} has an index; drop it before changing the field's type"
                )));
            }
            if ix.unique && !field.strict {
                return Err(Error::InvalidSchema(format!(
                    "{path} has a unique index, which needs a strict field; drop it first"
                )));
            }
        }
        let already_enforced = old.is_some_and(|o| o.strict && o.field_type == field.field_type);
        if field.strict && !already_enforced {
            for bytes in self.scan_range(&entry.documents())?.values() {
                check_field(document::raw(bytes)?, path, field)?;
            }
            // A Document committed concurrently was not checked.
            self.depend_on_whole_collection(entry.prefix());
        }
        entry.schema.insert(path.clone(), field);
        self.write_collection_entry(&entry)
    }

    /// Remove a field's declaration from the Collection Schema. Refused
    /// while an Index on the field exists.
    pub fn remove_field_type(
        &mut self,
        collection: &CollectionPath,
        path: &FieldPath,
    ) -> Result<()> {
        let mut entry = self.collection_entry(collection)?;
        if entry.index_on(path).is_some() {
            return Err(Error::InvalidSchema(format!(
                "{path} has an index; drop it before removing the field's type"
            )));
        }
        if entry.schema.remove(path).is_none() {
            return Err(Error::InvalidSchema(format!("{path} is not declared")));
        }
        self.write_collection_entry(&entry)
    }

    /// The Collection Schema: every declared field path and its type.
    pub fn collection_schema(
        &mut self,
        collection: &CollectionPath,
    ) -> Result<BTreeMap<FieldPath, FieldSchema>> {
        Ok(self.collection_entry(collection)?.schema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_fields_accept_only_exact_values() {
        use FieldType::*;
        assert!(fits_exactly(&Bson::Double(30.0), Int));
        assert!(!fits_exactly(&Bson::Double(30.5), Int));
        assert!(!fits_exactly(&Bson::Double(1e300), Int));
        assert!(fits_exactly(&Bson::Int64(1 << 53), Double));
        assert!(!fits_exactly(&Bson::Int64((1 << 53) + 1), Double));
        assert!(!fits_exactly(&Bson::Double(f64::NAN), Double));
        assert!(fits_exactly(&Bson::Double(f64::INFINITY), Double));
        assert!(!fits_exactly(&Bson::String("x".into()), Int));
        assert!(fits_exactly(&Bson::String("x".into()), String));
        assert!(!fits_exactly(&Bson::Null, String));
    }
}
