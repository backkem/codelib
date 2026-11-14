//! Host calls: give scripts a typed API over your own state.
//!
//! A `Router` of Rust handlers answers the script's `host.call(...)`. The
//! JavaScript client and the API reference are generated from the router,
//! and the state goes into each run and comes back out.
//!
//! ```text
//! cargo run --example notes
//! ```

use codelib::rpc::codegen::{self, JsOptions};
use codelib::{Failure, Router, Sandbox};
use serde::{Deserialize, Serialize};

/// The state scripts work on. It lives on the host; scripts only reach it
/// through the methods below.
#[derive(Debug, Default)]
struct Notebook {
    notes: Vec<Note>,
    read_only: bool,
}

#[derive(Clone, Debug, Serialize)]
struct Note {
    id: usize,
    text: String,
    done: bool,
}

#[derive(Deserialize)]
struct Add {
    text: String,
}

#[derive(Deserialize)]
struct ById {
    id: usize,
}

fn router() -> Router<Notebook> {
    let mut rpc = Router::<Notebook>::new();

    rpc.method("notes.add", |book: &mut Notebook, add: Add| {
        let note = Note {
            id: book.notes.len() + 1,
            text: add.text,
            done: false,
        };
        book.notes.push(note.clone());
        Ok(note)
    })
    .summary("Add a note.")
    .args("{ text }")
    .returns("{ id, text, done }")
    .tag("write");

    rpc.method("notes.done", |book: &mut Notebook, by: ById| {
        let note = book
            .notes
            .iter_mut()
            .find(|note| note.id == by.id)
            .ok_or_else(|| {
                Failure::new("not-found", format!("no note {}", by.id))
                    .with_hint("notes.list shows the ids")
            })?;
        note.done = true;
        Ok(note.clone())
    })
    .summary("Mark a note as done.")
    .args("{ id }")
    .returns("{ id, text, done }")
    .tag("write");

    rpc.method("notes.list", |book: &mut Notebook, _: ()| {
        Ok(book.notes.clone())
    })
    .summary("All notes.")
    .returns("[{ id, text, done }]");

    // One guard covers every method tagged `write`.
    rpc.guard(|method, book: &Notebook| {
        if book.read_only && method.has_tag("write") {
            return Err(Failure::denied(format!(
                "{} needs write access",
                method.name
            )));
        }
        Ok(())
    });

    rpc
}

const PROGRAM: &str = r#"
api.notes.add({ text: "write the design" });
const build = api.notes.add({ text: "build it" });
api.notes.done({ id: build.id });

try {
    api.notes.done({ id: 99 });
} catch (error) {
    console.log(`caught ${error.code}: ${error.message} (${error.hint})`);
}

return api.notes.list().filter((note) => !note.done).map((note) => note.text);
"#;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc = router();

    // Both generated from the router, so they cannot drift from the handlers.
    let client = codegen::js_client(rpc.methods(), &JsOptions::new("api"));
    println!("{}", codegen::markdown(rpc.methods(), "Notes API"));

    let sandbox = Sandbox::<Notebook>::builder()
        .prelude("api.js", client)
        .host_calls(rpc)
        .build()?;

    // The notebook goes in, the script works on it, and it comes back.
    let outcome = sandbox.script(PROGRAM).run_with(Notebook::default()).await;
    print!("{}", outcome.console.stdout);
    println!("still open: {}", outcome.result?);
    println!("notebook now holds {} notes", outcome.state.notes.len());

    // Same sandbox, a read-only notebook: the guard refuses the write.
    let outcome = sandbox
        .script(r#"api.notes.add({ text: "sneaky" })"#)
        .run_with(Notebook {
            read_only: true,
            ..outcome.state
        })
        .await;
    println!("read-only run: {}", outcome.result.unwrap_err());
    println!("notebook still holds {} notes", outcome.state.notes.len());

    Ok(())
}
