//! `shellctl.exe` — control a running Slowshell from a terminal.
//!
//! Every subcommand is one line of JSON over a named pipe, so the shell never
//! needs a second binary to talk to and the protocol stays in one file
//! ([`slowshell_core::ipc`]). This crate is only the human-facing half: argument
//! parsing, and printing whatever comes back in a way that reads in a terminal.
//!
//! ```text
//!   shellctl status              what is running
//!   shellctl reload              re-read the config
//!   shellctl stop                quit
//!   shellctl open launcher       show a named surface
//!   shellctl screens             the displays the shell sees
//!   shellctl diagnostics         the last build's problems
//!   shellctl graph               the reactive graph's size
//!   shellctl logs [--level info] [--limit 40]
//!   shellctl doctor              the environment, without a running shell
//!   shellctl actions             what a config may call
//! ```

use std::process::ExitCode;

use slowshell_core::ipc::{Request, Response};
use slowshell_win::ipc_pipe;

/// How long to wait for the shell to answer. Generous, because the shell's own
/// loop sleeps up to 50 ms and a slower machine should not look like a hang.
const TIMEOUT_MS: u32 = 2_000;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let rest: Vec<String> = args.iter().filter(|a| *a != "--json").cloned().collect();

    match run(&rest) {
        Ok(outcome) => {
            match outcome {
                Outcome::Print(text) => println!("{text}"),
                Outcome::Request { request, render } => {
                    if json {
                        // The raw envelope, for a script that wants the fields.
                        match ipc_pipe::request(&request, TIMEOUT_MS) {
                            Ok(r) => println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default()),
                            Err(e) => {
                                eprintln!("shellctl: {e}");
                                return ExitCode::from(2);
                            }
                        }
                    } else {
                        return send_and_print(&request, render);
                    }
                }
                Outcome::Help => {
                    print_help();
                    return ExitCode::SUCCESS;
                }
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("shellctl: {message}");
            print_tip(&message);
            ExitCode::from(2)
        }
    }
}

/// What a parsed command line asks for.
enum Outcome {
    /// Print this and stop; nothing is sent.
    Print(String),
    /// Send this and render the answer.
    Request { request: Request, render: fn(&serde_json::Value) -> String },
    /// The help text.
    Help,
}

fn run(args: &[String]) -> Result<Outcome, String> {
    let Some(first) = args.first() else {
        return Ok(Outcome::Help);
    };
    match first.as_str() {
        "-h" | "--help" | "help" => Ok(Outcome::Help),
        "-V" | "--version" | "version" => {
            Ok(Outcome::Print(format!("shellctl {} (slowshell {})", env!("CARGO_PKG_VERSION"), slowshell_core::VERSION)))
        }
        "status" => Ok(Outcome::Request { request: Request::Status, render: render_status }),
        "reload" => Ok(Outcome::Request { request: Request::Reload, render: render_reload }),
        "stop" | "quit" => Ok(Outcome::Request { request: Request::Stop, render: render_reload }),
        "overlay" => {
            Ok(Outcome::Request { request: Request::ToggleOverlay, render: render_overlay })
        }
        "diagnostics" => {
            Ok(Outcome::Request { request: Request::Diagnostics, render: render_diagnostics })
        }
        "graph" => Ok(Outcome::Request { request: Request::Graph, render: render_graph }),
        "screens" => Ok(Outcome::Request { request: Request::Screens, render: render_screens }),
        "open" => {
            let name = args.get(1).ok_or_else(|| {
                "`open` needs the name of a surface, for example: shellctl open launcher".to_string()
            })?;
            Ok(Outcome::Request { request: Request::Open { name: name.clone() }, render: render_reload })
        }
        "logs" => {
            let limit = flag_value(args, "--limit")
                .map(|v| v.parse::<usize>().map_err(|e| format!("--limit: {e}")))
                .transpose()?
                .unwrap_or(40);
            let level = flag_value(args, "--level").unwrap_or_else(|| "info".into());
            Ok(Outcome::Request {
                request: Request::Logs { limit, level },
                render: render_logs,
            })
        }
        "doctor" => Ok(Outcome::Print(doctor())),
        "actions" => Ok(Outcome::Print(actions())),
        other => Err(format!(
            "`{other}` is not a command. Run `shellctl --help` for the list."
        )),
    }
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn send_and_print(request: &Request, render: fn(&serde_json::Value) -> String) -> ExitCode {
    match ipc_pipe::request(request, TIMEOUT_MS) {
        Ok(Response::Ok { data }) => {
            println!("{}", render(&data));
            ExitCode::SUCCESS
        }
        Ok(Response::Err { message }) => {
            eprintln!("shellctl: the shell said: {message}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("shellctl: {e}");
            print_tip(&e);
            ExitCode::from(2)
        }
    }
}

/// A hint for the two failures a user actually hits.
fn print_tip(message: &str) {
    if message.contains("no slowshell is running") {
        eprintln!("  Start it with `Shell.exe`, or run `Shell.exe <config>` for a one-off.");
    }
}

/// A reply's `data`, addressed by key.
///
/// Reads go through here rather than through `unwrap_or_default` at every call
/// site, so a field the shell stops sending renders as `?` in one obvious place
/// instead of as a panic in a terminal.
mod j {
    use serde_json::Value;

    pub fn field<'a>(v: &'a Value, key: &str) -> &'a Value {
        v.get(key).unwrap_or(&Value::Null)
    }

    /// A field as text, without JSON's quotes around strings.
    ///
    /// `Value::to_string` on a string yields `"hello"`, which is right for a
    /// machine and wrong for a person reading a status line.
    pub fn text(v: &Value, key: &str) -> String {
        match field(v, key) {
            Value::Null => "?".into(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }

    pub fn flag(v: &Value, key: &str) -> bool {
        field(v, key).as_bool().unwrap_or(false)
    }

    pub fn number(v: &Value, key: &str) -> i64 {
        field(v, key).as_i64().unwrap_or(0)
    }

    pub fn list<'a>(v: &'a Value, key: &str) -> &'a [Value] {
        field(v, key).as_array().map(|a| a.as_slice()).unwrap_or(&[])
    }

    /// One element of a list, as unquoted text.
    pub fn item(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }
}

fn render_status(v: &serde_json::Value) -> String {
    let mut out = format!("slowshell {}\n", j::text(v, "version"));
    out.push_str(&format!("  pid       {}\n", j::text(v, "pid")));
    out.push_str(&format!("  config    {}\n", j::text(v, "config")));
    out.push_str(&format!(
        "  panels    {}  ({} elements)\n",
        j::text(v, "panels"),
        j::text(v, "elements")
    ));
    out.push_str(&format!("  adapter   {}\n", j::text(v, "adapter")));
    out.push_str(&format!("  presents  {}\n", j::text(v, "presentation")));
    out.push_str(&format!("  uptime    {}s\n", j::text(v, "uptimeSeconds")));
    out.push_str(&format!("  overlay   {}\n", j::flag(v, "overlay")));
    let files = j::list(v, "watching");
    if !files.is_empty() {
        out.push_str("  watching\n");
        for f in files {
            out.push_str(&format!("    {}\n", j::item(f)));
        }
    }
    out.trim_end().to_string()
}

fn render_reload(v: &serde_json::Value) -> String {
    if j::flag(v, "reloading") {
        "reloading; the shell keeps running if the config is broken".into()
    } else if j::flag(v, "stopping") {
        "stopping".into()
    } else {
        let opened = j::text(v, "opened");
        if opened == "?" || opened.is_empty() {
            "done".into()
        } else {
            format!("opened `{opened}`")
        }
    }
}

fn render_overlay(v: &serde_json::Value) -> String {
    format!("overlay {}", if j::flag(v, "overlay") { "on" } else { "off" })
}

fn render_diagnostics(v: &serde_json::Value) -> String {
    let count = j::number(v, "count");
    if count == 0 {
        return "no problems in the current build".into();
    }
    let mut out = format!("{count} problem(s):\n");
    for d in j::list(v, "diagnostics") {
        let file = j::text(d, "file");
        let line = j::text(d, "line");
        let where_ = if file == "?" || file.is_empty() { "config".to_string() } else { format!("{file}:{line}") };
        out.push_str(&format!("  {}  {}\n", j::text(d, "severity"), where_));
        out.push_str(&format!("    {}\n", j::text(d, "message")));
        for h in j::list(d, "hints") {
            out.push_str(&format!("    did you mean: {h}\n"));
        }
        for n in j::list(d, "notes") {
            out.push_str(&format!("    {n}\n"));
        }
    }
    out.trim_end().to_string()
}

fn render_graph(v: &serde_json::Value) -> String {
    format!(
        "{} nodes ({} source, {} derived)\n{} edges, {} evaluations, {} waiting",
        j::text(v, "nodes"),
        j::text(v, "sources"),
        j::text(v, "derived"),
        j::text(v, "edges"),
        j::text(v, "recomputes"),
        j::text(v, "pending")
    )
}

fn render_screens(v: &serde_json::Value) -> String {
    let list = j::list(v, "screens");
    if list.is_empty() {
        return "no displays".into();
    }
    let mut out = String::new();
    for s in list {
        out.push_str(&format!(
            "  {:<8} {:>5}x{:<5} @{}  origin {},{}  {}Hz{}\n",
            j::text(s, "id"),
            j::text(s, "width"),
            j::text(s, "height"),
            j::text(s, "scale"),
            j::text(s, "x"),
            j::text(s, "y"),
            j::text(s, "refreshHz"),
            if j::flag(s, "primary") { "  primary" } else { "" }
        ));
        // The work area is what maximized windows are given. A panel with
        // `exclusive: true` shrinks it, and this is where you see that.
        out.push_str(&format!(
            "  {:<8} work area {},{} {}x{}\n",
            "",
            j::text(s, "workX"),
            j::text(s, "workY"),
            j::text(s, "workWidth"),
            j::text(s, "workHeight")
        ));
    }
    // What this shell reserved, which is exact. The work area above is the
    // authoritative answer to "is space actually reserved" — it comes from
    // `GetMonitorInfo`, re-read on every call.
    let reserved = j::list(v, "reserved");
    if !reserved.is_empty() {
        out.push_str("  reserved by slowshell\n");
        for r in reserved {
            out.push_str(&format!(
                "    {:<14} {:<8} {:>5},{:<5} {}x{}\n",
                j::text(r, "panel"),
                j::text(r, "edge"),
                j::text(r, "x"),
                j::text(r, "y"),
                j::text(r, "width"),
                j::text(r, "height")
            ));
            // Said plainly, because "my bar does not slide away" is otherwise
            // indistinguishable from a bug.
            if j::flag(r, "autoHideRequested") {
                out.push_str(
                    "                 the OS was asked to auto-hide this edge; \
                     auto-hide is not implemented, so the bar stays put\n",
                );
            }
        }
    }
    out.trim_end().to_string()
}

fn render_logs(v: &serde_json::Value) -> String {
    let list = j::list(v, "records");
    if list.is_empty() {
        return "nothing logged at that level".into();
    }
    let mut out = String::new();
    // Newest first on the wire, oldest first on screen: a log reads forwards.
    for r in list.iter().rev() {
        out.push_str(&format!("{:<5} {}: {}\n", j::text(r, "level"), j::text(r, "target"), j::text(r, "message")));
    }
    out.trim_end().to_string()
}

/// The environment report, computed here so it works with no shell running.
fn doctor() -> String {
    use slowshell_win::{Graphics, Monitors};
    let mut out = String::new();
    out.push_str(&format!("Slowshell {}\n\n", slowshell_core::VERSION));
    out.push_str("Displays\n");
    for m in &Monitors::enumerate().list {
        out.push_str(&format!(
            "  {:<8} {:>5}x{:<5} @{:>3}  origin {},{}  {}Hz{}\n",
            m.id,
            m.width,
            m.height,
            format!("{:.0}x", m.scale),
            m.x,
            m.y,
            m.refresh_hz,
            if m.primary { "  primary" } else { "" }
        ));
    }
    out.push_str("\nGraphics\n");
    match Graphics::new().and_then(|g| g.create().map(|()| g)) {
        Ok(g) => {
            out.push_str(&format!("  adapter      {}\n", g.adapter_name()));
            out.push_str(&format!(
                "  renderer     {}\n",
                if g.software() { "software (WARP)" } else { "hardware" }
            ));
            out.push_str(&format!("  presentation {}\n", g.presentation().label()));
            if !g.presentation().honours_alpha() {
                out.push_str("  alpha        discarded at present; translucent colours are pre-composited\n");
            }
        }
        Err(e) => out.push_str(&format!("  device       unavailable: {e}\n")),
    }
    out.push_str(&format!(
        "  control pipe {}\n",
        ipc_pipe::pipe_name()
    ));
    out.push_str(&format!(
        "  backdrop     {}\n",
        if slowshell_win::backdrop::supported() { "available" } else { "not available" }
    ));
    out.push_str("\nConfig\n");
    let path = slowshell_core::paths::shell_config();
    out.push_str(&format!("  path         {}\n", path.display()));
    out.push_str(&format!("  exists       {}\n", path.exists()));
    match slowshell_runtime_build(&path) {
        Ok(summary) => out.push_str(&format!("  build        {summary}\n")),
        Err(e) => out.push_str(&format!("  build        failed: {e}\n")),
    }
    out.trim_end().to_string()
}

/// Compile the config without a shell, for `doctor`.
///
/// Returns the summary on success and the first problem on failure, because a
/// report that says "failed" without saying why sends the user to the log.
fn slowshell_runtime_build(path: &std::path::Path) -> Result<String, String> {
    use slowshell_runtime::Runtime;
    if !path.exists() {
        return Err(format!("no config at {}", path.display()));
    }
    let mut rt = Runtime::new();
    rt.seed_sources();
    let outcome = rt.build(path);
    if outcome.compiled.is_none() {
        return Err(outcome
            .diagnostics
            .first()
            .map(|d| d.message.clone())
            .unwrap_or_else(|| "unknown error".into()));
    }
    Ok(outcome.summary())
}

/// Every action a config may call.
///
/// Built by registering the Windows half into an empty registry, so the list
/// cannot drift from what the shell actually answers to.
fn actions() -> String {
    let mut registry = slowshell_core::Registry::new();
    slowshell_win::platform::register(&mut registry);
    // The shell's own actions are registered in the binary, so the list here is
    // the platform half. Saying so beats printing a list that is quietly short.
    let mut out = String::from("Platform actions\n\n");
    for line in registry.describe() {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("\nAlso available from the shell: shell.reload, shell.stop, shell.overlay,\n  shell.open(name), clipboard.set(text), clipboard.get(), notify(title, body),\n  launch(target), exec.run(program, args)\n");
    out.push_str("\n`shellctl status` lists everything the running shell has registered.\n");
    out.trim_end().to_string()
}

fn print_help() {
    println!(
        "shellctl {} — control a running Slowshell

USAGE:
    shellctl <command> [options]

COMMANDS:
    status         What is running: pid, config, panels, adapter, uptime
    reload         Re-read the config and rebuild the scene
    stop           Quit the shell
    open <name>    Show a named surface, e.g. the launcher
    overlay        Turn the developer error overlay on or off
    diagnostics    Problems from the last build, with file and line
    graph          The size of the reactive graph
    screens        The displays the shell sees, with scale and refresh rate
    logs           Recent log records
    doctor         The environment, without needing a running shell
    actions        Everything a config may call

OPTIONS:
    --limit <n>    With `logs`: how many records. Default 40.
    --level <lvl>  With `logs`: error, warn, info, debug or trace.
    --json         Print the raw reply instead of a formatted one.
    -h, --help     This text.
    -V, --version  The version.

EXAMPLES:
    shellctl status
    shellctl reload && shellctl diagnostics
    shellctl logs --level warn --limit 10
    shellctl open launcher",
        env!("CARGO_PKG_VERSION")
    );
}
