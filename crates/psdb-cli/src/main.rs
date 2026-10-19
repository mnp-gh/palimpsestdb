//! `psdb`: an interactive shell over an embedded, in-memory PalimpsestDB.
//!
//! Values are written as MongoDB relaxed Extended JSON, e.g.
//! `{"_id": {"$oid": "65f0..."}, "n": 1}`. Everything is lost on exit.

use std::io::{self, BufRead, IsTerminal, Write};

use bson::{Bson, Document};
use psdb_core::{Database, Transaction};
use psdb_storage_memory::MemoryEngine;

type Db = Database<MemoryEngine>;
type Txn<'db> = Transaction<'db, MemoryEngine>;

const HELP: &str = "\
Databases and collections
  dbs                          list databases
  create-db <db>               create a database
  drop-db <db>                 drop a database
  colls <db>                   list collections
  create-coll <db>.<coll>      create a collection
  drop-coll <db>.<coll>        drop a collection

Documents (values are relaxed Extended JSON)
  insert <db>.<coll> <doc>             insert; prints the _id
  get <db>.<coll> <id>                 print the document with this _id
  replace <db>.<coll> <id> <doc>       replace the whole document
  delete <db>.<coll> <id>              delete the document
  find <db>.<coll> [filter]            print matching documents

Transactions (without begin, every command commits on its own)
  begin                        start a transaction
  commit                       commit it
  abort                        discard it

  help                         show this help
  quit                         exit (all data is lost)";

enum Outcome {
    Continue,
    Quit,
}

fn main() {
    let db = Database::new(MemoryEngine::new());
    let mut txn: Option<Txn<'_>> = None;
    let interactive = io::stdin().is_terminal();
    if interactive {
        println!(
            "PalimpsestDB v{} (in-memory). Type `help` for commands.",
            env!("CARGO_PKG_VERSION")
        );
    }

    let mut line = String::new();
    loop {
        if interactive {
            print!(
                "{}",
                if txn.is_some() {
                    "psdb(txn)> "
                } else {
                    "psdb> "
                }
            );
            io::stdout().flush().expect("stdout");
        }
        line.clear();
        match io::stdin().lock().read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("error: reading input: {e}");
                break;
            }
        }
        match execute(&db, &mut txn, line.trim()) {
            Ok(Outcome::Continue) => {}
            Ok(Outcome::Quit) => break,
            Err(message) => eprintln!("error: {message}"),
        }
    }
}

fn execute<'db>(db: &'db Db, txn: &mut Option<Txn<'db>>, line: &str) -> Result<Outcome, String> {
    let (command, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    match command {
        "" => {}
        "help" => println!("{HELP}"),
        "quit" | "exit" => return Ok(Outcome::Quit),

        "begin" => {
            if txn.is_some() {
                return Err("a transaction is already open; commit or abort it first".into());
            }
            *txn = Some(db.begin());
        }
        "commit" => {
            let open = txn.take().ok_or("no open transaction")?;
            let ts = open.commit().map_err(|e| e.to_string())?;
            println!("committed at {ts}");
        }
        "abort" => {
            txn.take().ok_or("no open transaction")?.abort();
            println!("aborted");
        }

        "dbs" => print_names(&with_txn(db, txn, |t| t.list_databases())?),
        "create-db" => with_txn(db, txn, |t| t.create_database(rest))?,
        "drop-db" => with_txn(db, txn, |t| t.drop_database(rest))?,
        "colls" => print_names(&with_txn(db, txn, |t| t.list_collections(rest))?),
        "create-coll" => {
            let (d, c) = namespace(rest)?;
            with_txn(db, txn, |t| t.create_collection(d, c))?;
        }
        "drop-coll" => {
            let (d, c) = namespace(rest)?;
            with_txn(db, txn, |t| t.drop_collection(d, c))?;
        }

        "insert" => {
            let (ns, args) = split_namespace(rest)?;
            let [doc] = values::<1>(args)?;
            let doc = as_document(doc)?;
            let id = with_txn(db, txn, |t| t.insert(ns.0, ns.1, doc))?;
            println!("{}", to_json(id));
        }
        "get" => {
            let (ns, args) = split_namespace(rest)?;
            let [id] = values::<1>(args)?;
            match with_txn(db, txn, |t| t.get(ns.0, ns.1, &id))? {
                Some(doc) => println!("{}", to_json(Bson::Document(doc))),
                None => println!("not found"),
            }
        }
        "replace" => {
            let (ns, args) = split_namespace(rest)?;
            let [id, doc] = values::<2>(args)?;
            let doc = as_document(doc)?;
            let replaced = with_txn(db, txn, |t| t.replace(ns.0, ns.1, &id, doc))?;
            println!("{}", if replaced { "replaced" } else { "not found" });
        }
        "delete" => {
            let (ns, args) = split_namespace(rest)?;
            let [id] = values::<1>(args)?;
            let deleted = with_txn(db, txn, |t| t.delete(ns.0, ns.1, &id))?;
            println!("{}", if deleted { "deleted" } else { "not found" });
        }
        "find" => {
            let (ns, args) = split_namespace(rest)?;
            let filter = if args.is_empty() {
                Document::new()
            } else {
                as_document(values::<1>(args)?[0].clone())?
            };
            let docs = with_txn(db, txn, |t| t.find(ns.0, ns.1, &filter))?;
            for doc in &docs {
                println!("{}", to_json(Bson::Document(doc.clone())));
            }
            println!("({} found)", docs.len());
        }

        other => return Err(format!("unknown command {other:?}; type `help`")),
    }
    Ok(Outcome::Continue)
}

/// Run `work` in the open transaction, or in its own autocommit transaction
/// if none is open.
fn with_txn<'db, T>(
    db: &'db Db,
    txn: &mut Option<Txn<'db>>,
    work: impl FnOnce(&mut Transaction<'_, MemoryEngine>) -> psdb_core::Result<T>,
) -> Result<T, String> {
    match txn {
        Some(open) => work(open),
        None => db.run(work),
    }
    .map_err(|e| e.to_string())
}

fn namespace(text: &str) -> Result<(&str, &str), String> {
    text.split_once('.')
        .ok_or_else(|| format!("expected <db>.<coll>, got {text:?}"))
}

/// Split `<db>.<coll> rest...` into the namespace and the rest.
fn split_namespace(text: &str) -> Result<((&str, &str), &str), String> {
    let (ns, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    Ok((namespace(ns)?, rest.trim()))
}

/// Parse exactly `N` whitespace-separated Extended JSON values.
fn values<const N: usize>(text: &str) -> Result<[Bson; N], String> {
    let mut stream = serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>();
    let mut parsed = Vec::new();
    loop {
        // Where the next argument starts, for pointing at it in errors.
        let start = stream.byte_offset();
        let Some(next) = stream.next() else { break };
        let argument = parsed.len() + 1;
        let json = next.map_err(|e| {
            let token = text[start..].split_whitespace().next().unwrap_or("");
            format!(
                "argument {argument} is not valid JSON ({e}) at `{token}`\n\
                 hint: quote strings (\"abc\"); write ObjectIds as {{\"$oid\": \"6ad6...\"}}"
            )
        })?;
        let value = Bson::try_from(json)
            .map_err(|e| format!("argument {argument} is not valid Extended JSON: {e}"))?;
        parsed.push(value);
    }
    let count = parsed.len();
    parsed
        .try_into()
        .map_err(|_| format!("expected {N} JSON value(s), got {count}"))
}

fn as_document(value: Bson) -> Result<Document, String> {
    match value {
        Bson::Document(doc) => Ok(doc),
        other => Err(format!("expected a JSON object, got {other}")),
    }
}

fn to_json(value: Bson) -> String {
    value.into_relaxed_extjson().to_string()
}

fn print_names(names: &[String]) {
    for name in names {
        println!("{name}");
    }
}
