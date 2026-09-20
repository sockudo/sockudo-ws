use anyhow::{Context, Result};
use autobahn_testsuite::{
    catalog,
    config::Spec,
    service::{self, Mode},
};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "wstest",
    version,
    about = "Native Rust Autobahn WebSocket conformance suite"
)]
struct Args {
    #[arg(short, long, value_enum)]
    mode: Option<Mode>,
    #[arg(short, long)]
    spec: Option<std::path::PathBuf>,
    #[arg(short = 'w', long)]
    wsuri: Option<String>,
    #[arg(short = 'i', long, default_value = "autobahn-testsuite-rs")]
    ident: String,
    #[arg(short = 'o', long)]
    outfile: Option<std::path::PathBuf>,
    #[arg(long)]
    list_cases: bool,
    #[arg(short = 'a', long)]
    autobahnversion: bool,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    cases: Vec<String>,
    #[arg(long)]
    concurrency: Option<usize>,
    #[arg(long)]
    outdir: Option<String>,
    #[arg(short = 'k', long)]
    key: Option<String>,
    #[arg(short = 'c', long)]
    cert: Option<String>,
    #[arg(long)]
    ca: Option<String>,
    #[arg(long)]
    message_count: Option<usize>,
    #[arg(long)]
    case_timeout_ms: Option<u64>,
    #[arg(long)]
    connections: Option<usize>,
    #[arg(long)]
    hold_ms: Option<u64>,
    #[arg(short = 'u', long)]
    webport: Option<u16>,
}
#[tokio::main]
async fn main() -> Result<std::process::ExitCode> {
    let args = Args::parse();
    if args.autobahnversion {
        println!(
            "autobahn-testsuite-rs {}\nUpstream {}",
            env!("CARGO_PKG_VERSION"),
            catalog::UPSTREAM_COMMIT
        );
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let mut spec = if let Some(path) = args.spec {
        Spec::load(path).context("loading test specification")?
    } else {
        Spec::default()
    };
    if let Some(url) = args.wsuri {
        spec.url = url;
    }
    if !args.cases.is_empty() {
        spec.cases = args.cases;
    }
    if let Some(n) = args.concurrency {
        spec.concurrency = n;
    }
    if let Some(out) = args.outdir {
        spec.outdir = out;
    }
    if args.key.is_some() {
        spec.key = args.key;
    }
    if args.cert.is_some() {
        spec.cert = args.cert;
    }
    if args.ca.is_some() {
        spec.ca = args.ca;
    }
    if args.message_count.is_some() {
        spec.message_count = args.message_count;
    }
    if args.case_timeout_ms.is_some() {
        spec.case_timeout_ms = args.case_timeout_ms;
    }
    if let Some(n) = args.connections {
        spec.connections = n;
    }
    if let Some(n) = args.hold_ms {
        spec.hold_ms = n;
    }
    if let Some(port) = args.webport {
        spec.webport = port;
    }
    spec.validate()?;
    if args.list_cases {
        let cases = catalog::load()?;
        let selected = spec.selected(&cases);
        if args.json {
            println!("{}", serde_json::to_string_pretty(&selected)?);
        } else {
            for (i, case) in selected.iter().enumerate() {
                println!(
                    "{}\t{}\t{}",
                    i + 1,
                    case.id,
                    case.description.replace('\n', " ")
                );
            }
        }
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let mode = args.mode.context("provide --mode or --list-cases")?;
    if mode == Mode::Serializer {
        let path = args.outfile.context("serializer requires --outfile")?;
        autobahn_testsuite::serializer::write(path)?;
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let failed = service::run(mode, spec, args.ident).await?;
    Ok(if failed == 0 {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    })
}
