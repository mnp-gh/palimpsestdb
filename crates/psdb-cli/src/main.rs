//! `psdb`: an interactive shell over an embedded, in-memory PalimpsestDB.
//!
//! Values are written as MongoDB relaxed Extended JSON, e.g.
//! `{"_id": {"$uuid": "0192f3a8-..."}, "n": 1}`. Everything is lost on exit.

use std::io::{self, BufRead, IsTerminal};

use bson::{Binary, Bson, Document, spec::BinarySubtype};
use psdb_core::{Database, Transaction};
use psdb_storage_memory::MemoryEngine;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use serde_json::{Value, json};
use uuid::Uuid;

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
    let mut input = Input::open();
    if matches!(input, Input::Terminal(_)) {
        println!(
            "PalimpsestDB v{} (in-memory). Type `help` for commands.",
            env!("CARGO_PKG_VERSION")
        );
    }

    loop {
        let prompt = if txn.is_some() {
            "psdb(txn)> "
        } else {
            "psdb> "
        };
        let Some(line) = input.next_line(prompt) else {
            break;
        };
        match execute(&db, &mut txn, line.trim()) {
            Ok(Outcome::Continue) => {}
            Ok(Outcome::Quit) => break,
            Err(message) => eprintln!("error: {message}"),
        }
    }
}

/// Where commands come from: a line editor with history on a terminal,
/// plain lines when input is piped (`psdb < script.txt`).
enum Input {
    Terminal(Box<DefaultEditor>),
    Piped(io::StdinLock<'static>),
}

impl Input {
    fn open() -> Self {
        if io::stdin().is_terminal() {
            match DefaultEditor::new() {
                Ok(editor) => return Input::Terminal(Box::new(editor)),
                Err(e) => eprintln!("warning: line editing unavailable: {e}"),
            }
        }
        Input::Piped(io::stdin().lock())
    }

    /// The next line, or `None` at end of input (Ctrl-D).
    fn next_line(&mut self, prompt: &str) -> Option<String> {
        match self {
            Input::Terminal(editor) => loop {
                match editor.readline(prompt) {
                    Ok(line) => {
                        if !line.trim().is_empty() {
                            // Failing to record history is harmless.
                            let _ = editor.add_history_entry(line.as_str());
                        }
                        return Some(line);
                    }
                    // Ctrl-C discards the current line, like most shells.
                    Err(ReadlineError::Interrupted) => continue,
                    Err(ReadlineError::Eof) => return None,
                    Err(e) => {
                        eprintln!("error: reading input: {e}");
                        return None;
                    }
                }
            },
            Input::Piped(stdin) => {
                let mut line = String::new();
                match stdin.read_line(&mut line) {
                    Ok(0) => None,
                    Ok(_) => Some(line),
                    Err(e) => {
                        eprintln!("error: reading input: {e}");
                        None
                    }
                }
            }
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
            with_txn(db, txn, |t: &mut Transaction<'_, MemoryEngine>| {
                t.drop_collection(d, c)
            })?;
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
                None => not_found(&id),
            }
        }
        "replace" => {
            let (ns, args) = split_namespace(rest)?;
            let [id, doc] = values::<2>(args)?;
            let doc = as_document(doc)?;
            let replaced = with_txn(db, txn, |t| t.replace(ns.0, ns.1, &id, doc))?;
            if replaced {
                println!("replaced");
            } else {
                not_found(&id);
            }
        }
        "delete" => {
            let (ns, args) = split_namespace(rest)?;
            let [id] = values::<1>(args)?;
            let deleted = with_txn(db, txn, |t| t.delete(ns.0, ns.1, &id))?;
            if deleted {
                println!("deleted");
            } else {
                not_found(&id);
            }
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
                 hint: quote strings (\"abc\"); write UUIDs as {{\"$uuid\": \"0192f3a8-...\"}}"
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
    to_json_value(value).to_string()
}

/// Relaxed Extended JSON, except UUIDs print as `{"$uuid": "..."}`, the
/// same form the shell accepts, rather than as base64 `$binary`.
fn to_json_value(value: Bson) -> Value {
    match value {
        Bson::Binary(Binary {
            subtype: BinarySubtype::Uuid,
            bytes,
        }) if bytes.len() == 16 => {
            json!({ "$uuid": Uuid::from_slice(&bytes).expect("16 bytes").to_string() })
        }
        Bson::Document(doc) => Value::Object(
            doc.into_iter()
                .map(|(k, v)| (k, to_json_value(v)))
                .collect(),
        ),
        Bson::Array(items) => Value::Array(items.into_iter().map(to_json_value).collect()),
        other => other.into_relaxed_extjson(),
    }
}

/// Report a missing Document, with a hint when the `_id` looks like a UUID
/// typed as a plain string.
fn not_found(id: &Bson) {
    println!("not found");
    if let Bson::String(text) = id
        && Uuid::try_parse(text).is_ok()
    {
        println!("hint: {text:?} is a string; for a UUID write {{\"$uuid\": \"{text}\"}}");
    }
}

fn print_names(names: &[String]) {
    for name in names {
        println!("{name}");
    }
}
