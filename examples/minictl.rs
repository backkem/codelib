//! minictl: a tiny "code mode" CLI.
//!
//! Runs a JavaScript program in the sandbox. The program can only reach the
//! hosts you allow, and tokens are attached on the host side: the script
//! never sees them.
//!
//! ```text
//! cargo run --example minictl -- -e 'return 6 * 7'
//!
//! GITHUB_TOKEN=... cargo run --example minictl -- \
//!     --allow api.github.com=GITHUB_TOKEN --read-only \
//!     -e 'const me = await github.get("/user"); return { login: me.login, repos: me.public_repos }'
//! ```

use std::io::Read as _;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use codelib::{Egress, EnvBearer, Rule, RunError, Sandbox};

const USAGE: &str = "\
usage: minictl [options] (-e CODE | FILE | -)

  -e CODE              the program (the body of an async function)
  FILE | -             read the program from a file or stdin
  --allow HOST[=VAR]   let fetch() reach HOST (e.g. api.github.com, *.example.com);
                       with =VAR, send the token in env var VAR as a bearer
                       credential. Repeatable.
  --read-only          allowed hosts accept GET and HEAD only
  --input JSON         data for the program, visible as host.input
  --timeout SECONDS    time budget (default 30)

exit codes: 0 ok, 1 the program threw, 2 usage, 3 timeout / out of memory / sandbox";

/// A provider in miniature: a client library the program gets for free. It
/// carries no credentials; the egress rule adds them.
const GITHUB_PRELUDE: &str = r#"
globalThis.github = {
    async get(path) {
        const response = await fetch("https://api.github.com" + path, {
            headers: { accept: "application/vnd.github+json" },
        });
        if (!response.ok) {
            throw new Error(`GitHub answered ${response.status}: ${await response.text()}`);
        }
        return response.json();
    },
};
"#;

struct Args {
    program: String,
    name: String,
    allow: Vec<(String, Option<String>)>,
    read_only: bool,
    input: serde_json::Value,
    timeout: Duration,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut program = None;
    let mut name = "program.js".to_string();
    let mut allow = Vec::new();
    let mut read_only = false;
    let mut input = serde_json::Value::Null;
    let mut timeout = Duration::from_secs(30);

    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => return Err(String::new()),
            "-e" => program = Some(value("-e")?),
            "--allow" => {
                let spec = value("--allow")?;
                match spec.split_once('=') {
                    Some((host, var)) => allow.push((host.to_string(), Some(var.to_string()))),
                    None => allow.push((spec, None)),
                }
            }
            "--read-only" => read_only = true,
            "--input" => {
                input = serde_json::from_str(&value("--input")?)
                    .map_err(|e| format!("--input is not JSON: {e}"))?;
            }
            "--timeout" => {
                let seconds: f64 = value("--timeout")?
                    .parse()
                    .map_err(|_| "--timeout needs a number of seconds".to_string())?;
                timeout = Duration::from_secs_f64(seconds);
            }
            "-" => {
                let mut code = String::new();
                std::io::stdin()
                    .read_to_string(&mut code)
                    .map_err(|e| format!("reading stdin: {e}"))?;
                program = Some(code);
            }
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            file => {
                program = Some(
                    std::fs::read_to_string(file).map_err(|e| format!("reading {file}: {e}"))?,
                );
                name = file.to_string();
            }
        }
    }

    let program = program.ok_or("no program given")?;
    Ok(Args {
        program,
        name,
        allow,
        read_only,
        input,
        timeout,
    })
}

fn egress(args: &Args) -> Egress {
    let mut builder = Egress::builder();
    for (host, token_var) in &args.allow {
        // Some APIs insist on a user agent; set one on the host side.
        let mut rule = Rule::host(host.clone()).header("user-agent", "minictl");
        if args.read_only {
            rule = rule.read_only();
        }
        if let Some(var) = token_var {
            rule = rule.credential(EnvBearer::new(var.clone()));
        }
        builder = builder.allow(rule);
    }
    builder.build()
}

/// Building the sandbox compiles the engine the first time; say so if it
/// takes a moment.
fn build_sandbox() -> Result<Sandbox, codelib::Error> {
    let done = Arc::new(AtomicBool::new(false));
    let watcher = done.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        if !watcher.load(Ordering::Relaxed) {
            eprintln!(
                "minictl: compiling the JavaScript engine (first run only, cached afterwards)..."
            );
        }
    });
    let sandbox = Sandbox::builder()
        .prelude("github.js", GITHUB_PRELUDE)
        .build();
    done.store(true, Ordering::Relaxed);
    sandbox
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("minictl: {message}\n");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let sandbox = match build_sandbox() {
        Ok(sandbox) => sandbox,
        Err(error) => {
            eprintln!("minictl: {error}");
            return ExitCode::from(3);
        }
    };

    let outcome = sandbox
        .script(args.program.clone())
        .name(args.name.clone())
        .input(&args.input)
        .egress(egress(&args))
        .timeout(args.timeout)
        .on_emit(|kind, item| eprintln!("[{kind}] {item}"))
        .run()
        .await;

    // The console goes to stderr so stdout carries only the result.
    eprint!("{}", outcome.console.stdout);
    eprint!("{}", outcome.console.stderr);

    match outcome.result {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).expect("JSON values serialize")
            );
            ExitCode::SUCCESS
        }
        Err(RunError::Script(error)) => {
            eprintln!("minictl: {error}");
            if let Some(stack) = error.stack {
                eprintln!("{stack}");
            }
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("minictl: {error}");
            ExitCode::from(3)
        }
    }
}
