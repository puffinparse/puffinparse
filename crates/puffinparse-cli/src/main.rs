//! `puffinparse` command-line interface.

mod bench;
mod mcp;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use puffinparse_core::{DocumentRequest, ExtractRequest, Mode, OutputFormat};

#[derive(Parser, Debug)]
#[command(name = "puffinparse", version, about = "One API for every OCR provider", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// `parse` mode: layout-aware markdown + typed blocks.
    Parse(ParseArgs),
    /// `ocr` mode: plain text with line/word boxes.
    Ocr(OcrArgs),
    /// `extract` mode: pull a JSON object out of a document with a schema.
    Extract(ExtractArgs),
    /// List providers, models, modes, pricing, and whether an API key is configured.
    Providers {
        /// Only show models that serve this mode (parse | ocr | extract).
        #[arg(long)]
        mode: Option<String>,
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Run and report the open OCR benchmark (uses `parse` mode).
    #[command(subcommand)]
    Bench(bench::BenchCommand),
    /// Run the HTTP gateway (virtual keys, budgets, fallbacks, metrics). See docs/SERVER.md.
    Serve {
        /// Config file (TOML). Defaults to ./puffinparse.toml when it exists, else built-in defaults.
        #[arg(short, long, env = "PUFFINPARSE_CONFIG")]
        config: Option<std::path::PathBuf>,
        /// Override server.host.
        #[arg(long)]
        host: Option<String>,
        /// Override server.port.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Run a Model Context Protocol server on stdio for coding agents (Claude Code, Cursor, Codex).
    ///
    /// Tools: parse, ocr, extract, list_models, compare. Provider keys are read from this process's
    /// environment and never returned. Documents go only to the providers of the models called.
    Mcp(McpArgs),
}

#[derive(clap::Args, Debug)]
struct McpArgs {
    /// Only allow these models: "provider/model", "provider/*" or a bare provider name.
    /// Repeatable or comma-separated. Default: every model.
    #[arg(long = "models", value_name = "MODEL", value_delimiter = ',')]
    models: Vec<String>,
    /// Whole-call timeout per provider call, in seconds.
    #[arg(long, default_value_t = 300.0)]
    timeout: f64,
    /// Retries on transient errors (429 / 5xx / network).
    #[arg(long, default_value_t = 2)]
    max_retries: u32,
    /// Only read local files inside this directory (paths are canonicalized, symlinks followed).
    /// Default: any file this process can read. URLs are not affected.
    #[arg(long, value_name = "DIR")]
    root: Option<std::path::PathBuf>,
}

/// Options shared by every mode.
#[derive(clap::Args, Debug)]
struct CommonArgs {
    /// Local file path or http(s) URL.
    input: String,
    /// Model as "<provider>/<model>", e.g. reducto/standard. It must support this subcommand's
    /// mode (parse | ocr | extract); `puffinparse providers --mode <mode>` lists the candidates.
    /// A bare provider name (e.g. "reducto") picks its default model for the mode.
    #[arg(short, long, default_value = "reducto")]
    model: String,
    /// 1-based page selection, e.g. "1-3,7".
    #[arg(short, long)]
    pages: Option<String>,
    /// Language hint (ISO 639-1), forwarded when supported.
    #[arg(short, long)]
    language: Option<String>,
    /// Provider-specific options as a JSON object.
    #[arg(long, value_name = "JSON")]
    options: Option<String>,
    /// Include the provider's raw payload (json output only).
    #[arg(long)]
    raw: bool,
    /// Whole-call timeout in seconds.
    #[arg(long, default_value_t = 300.0)]
    timeout: f64,
    /// Retries on transient errors.
    #[arg(long, default_value_t = 2)]
    max_retries: u32,
    /// Override the API key (otherwise read from the provider's env var).
    #[arg(long, env = "PUFFINPARSE_API_KEY", hide_env_values = true)]
    api_key: Option<String>,
    /// Override the provider base URL.
    #[arg(long)]
    base_url: Option<String>,
}

impl CommonArgs {
    fn into_request(self, output: OutputFormat) -> Result<DocumentRequest> {
        let mut req = DocumentRequest::from_str_input(&self.input)
            .model(&self.model)
            .timeout_secs(self.timeout)
            .max_retries(self.max_retries)
            .include_raw(self.raw)
            .output(output);
        if let Some(p) = self.pages {
            req = req.pages(p);
        }
        if let Some(l) = self.language {
            req = req.language(l);
        }
        if let Some(o) = self.options {
            let v: serde_json::Value = serde_json::from_str(&o).context("--options must be a JSON object")?;
            req = req.provider_options(v);
        }
        if let Some(k) = self.api_key {
            req = req.api_key(k);
        }
        if let Some(b) = self.base_url {
            req = req.base_url(b);
        }
        Ok(req)
    }
}

#[derive(clap::Args, Debug)]
struct ParseArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Output format.
    #[arg(short, long, value_enum, default_value_t = ParseFormat::Markdown)]
    format: ParseFormat,
    /// Render the JSON in a provider's own response shape instead of the unified one:
    /// reducto | extend | llamaparse | puffinparse. Only affects `--format json`.
    #[arg(long, value_name = "VENDOR")]
    output_format: Option<String>,
}

#[derive(clap::Args, Debug)]
struct OcrArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Output format: plain text, or the full TextResponse as JSON (text + lines + words).
    #[arg(short, long, value_enum, default_value_t = OcrFormat::Text)]
    format: OcrFormat,
}

#[derive(clap::Args, Debug)]
struct ExtractArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// JSON Schema for the object to extract: a path to a .json file, or inline JSON.
    #[arg(short, long, value_name = "FILE|JSON")]
    schema: String,
    /// Extra natural-language guidance for the extractor.
    #[arg(long)]
    instructions: Option<String>,
    /// Ask for per-field citations (page, box, source text) when the provider supports them.
    #[arg(long)]
    citations: bool,
    /// Render the JSON in a provider's own extract shape instead of the unified one:
    /// reducto | extend | llamaparse | puffinparse (best effort — see docs/COMPAT.md).
    #[arg(long, value_name = "VENDOR")]
    output_format: Option<String>,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ParseFormat {
    Markdown,
    Text,
    Json,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum OcrFormat {
    Text,
    Json,
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("PUFFINPARSE_LOG").unwrap_or_else(|_| EnvFilter::new("error"));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}

#[tokio::main]
async fn main() {
    init_logging();
    let cli = Cli::parse();
    let code = match run(cli).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e:#}");
            match e.downcast_ref::<puffinparse_core::Error>() {
                Some(le)
                    if matches!(
                        le.kind,
                        puffinparse_core::ErrorKind::UnsupportedModel | puffinparse_core::ErrorKind::Input
                    ) =>
                {
                    2
                }
                _ => 1,
            }
        }
    };
    std::process::exit(code);
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Parse(args) => parse(args).await,
        Command::Ocr(args) => ocr(args).await,
        Command::Extract(args) => extract(args).await,
        Command::Providers { mode, json } => providers(mode.as_deref(), json),
        Command::Bench(cmd) => bench::run(cmd).await,
        Command::Serve { config, host, port } => serve(config, host, port).await,
        Command::Mcp(args) => {
            let allow = mcp::AllowList::new(&args.models)?;
            let root = match args.root {
                Some(dir) => {
                    let canonical = std::fs::canonicalize(&dir)
                        .with_context(|| format!("--root {}: cannot resolve", dir.display()))?;
                    anyhow::ensure!(canonical.is_dir(), "--root {} is not a directory", dir.display());
                    Some(canonical)
                }
                None => None,
            };
            mcp::run_stdio(mcp::Config::new(allow, args.timeout, args.max_retries, root)).await
        }
    }
}

async fn serve(config: Option<std::path::PathBuf>, host: Option<String>, port: Option<u16>) -> Result<()> {
    let default = std::path::Path::new("puffinparse.toml");
    let mut cfg = match config {
        Some(p) => puffinparse_server::Config::load(&p).map_err(anyhow::Error::msg)?,
        None if default.exists() => puffinparse_server::Config::load(default).map_err(anyhow::Error::msg)?,
        None => puffinparse_server::Config::default(),
    };
    if let Some(h) = host {
        cfg.server.host = h;
    }
    if let Some(p) = port {
        cfg.server.port = p;
    }
    puffinparse_server::serve(cfg).await.map_err(anyhow::Error::msg)
}

async fn parse(args: ParseArgs) -> Result<()> {
    let output = if args.format == ParseFormat::Text { OutputFormat::Text } else { OutputFormat::Markdown };
    let mut req = args.common.into_request(output)?;
    let shape = native_shape(args.output_format, args.format == ParseFormat::Json)?;
    if let Some(f) = &shape {
        req = req.output_format(f.clone());
    }
    let resp = puffinparse_core::parse(req).await?;
    match args.format {
        ParseFormat::Markdown => println!("{}", resp.markdown),
        ParseFormat::Text => println!("{}", resp.text),
        ParseFormat::Json => match &shape {
            Some(f) => println!("{}", serde_json::to_string_pretty(&resp.to_format(f)?)?),
            None => println!("{}", serde_json::to_string_pretty(&resp)?),
        },
    }
    if args.format != ParseFormat::Json {
        summary(&resp.model, resp.usage.pages, resp.latency_ms, resp.cost_usd);
    }
    Ok(())
}

async fn ocr(args: OcrArgs) -> Result<()> {
    let req = args.common.into_request(OutputFormat::Text)?;
    let resp = puffinparse_core::ocr(req).await?;
    match args.format {
        OcrFormat::Text => {
            println!("{}", resp.text);
            summary(&resp.model, resp.usage.pages, resp.latency_ms, resp.cost_usd);
        }
        OcrFormat::Json => println!("{}", serde_json::to_string_pretty(&resp)?),
    }
    Ok(())
}

async fn extract(args: ExtractArgs) -> Result<()> {
    let schema = load_schema(&args.schema)?;
    let mut document = args.common.into_request(OutputFormat::Markdown)?;
    // `extract` always prints JSON, so a native shape is always meaningful here.
    let shape = native_shape(args.output_format, true)?;
    if let Some(f) = &shape {
        document = document.output_format(f.clone());
    }
    let mut req = ExtractRequest::new(document, schema).citations(args.citations);
    if let Some(i) = args.instructions {
        req = req.instructions(i);
    }
    let resp = puffinparse_core::extract(req).await?;
    match &shape {
        Some(f) => println!("{}", serde_json::to_string_pretty(&resp.to_format(f)?)?),
        None => println!("{}", serde_json::to_string_pretty(&resp)?),
    }
    Ok(())
}

/// Validate `--output-format` before any network call, and drop it (with a warning) when the
/// command is not printing JSON — the vendor shapes only exist as JSON.
fn native_shape(output_format: Option<String>, json_output: bool) -> Result<Option<String>> {
    let Some(raw) = output_format else { return Ok(None) };
    let format: puffinparse_core::OutputShape = raw.parse()?;
    if !json_output {
        eprintln!("warning: --output-format {format} only applies to --format json; ignoring it for this output");
        return Ok(None);
    }
    Ok(Some(format.to_string()))
}

/// `--schema` is either inline JSON (starts with `{`) or a path to a JSON file.
fn load_schema(arg: &str) -> Result<serde_json::Value> {
    let trimmed = arg.trim();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).context("--schema is not valid inline JSON");
    }
    let text = std::fs::read_to_string(trimmed).with_context(|| format!("cannot read schema file '{trimmed}'"))?;
    serde_json::from_str(&text).with_context(|| format!("schema file '{trimmed}' is not valid JSON"))
}

fn summary(model: &str, pages: u32, latency_ms: u64, cost_usd: Option<f64>) {
    eprintln!(
        "[{model}] {pages} page(s) in {latency_ms} ms{}",
        cost_usd.map(|c| format!(", est. ${c:.4}")).unwrap_or_default()
    );
}

fn providers(mode: Option<&str>, json: bool) -> Result<()> {
    let mode: Option<Mode> = match mode {
        Some(m) => Some(m.parse::<Mode>()?),
        None => None,
    };
    let prices = puffinparse_core::pricing::all_prices();
    let price_of = |model: &str, m: Mode| prices.get(model).and_then(|e| e.for_mode(m));
    let keep = |mi: &puffinparse_core::ModelInfo| mode.map(|m| mi.supports(m)).unwrap_or(true);

    let output_formats: Vec<&str> =
        puffinparse_core::OutputShape::ALL.iter().map(puffinparse_core::OutputShape::as_str).collect();

    if json {
        let providers: Vec<serde_json::Value> = puffinparse_core::PROVIDERS
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "display_name": p.display_name,
                    "env_var": p.env_var,
                    "key_configured": key_configured(p.env_var),
                    "key_required": p.key_required(),
                    "self_hosted": p.self_hosted(),
                    "base_url": p.base_url,
                    "docs": p.docs,
                    "models": p.models.iter().filter(|m| keep(m)).map(|m| {
                        let q = m.qualified();
                        serde_json::json!({
                            "model": q,
                            "default": m.default,
                            "description": m.description,
                            "modes": m.modes,
                            "per_page_usd": m.modes.iter()
                                .filter_map(|md| price_of(&q, *md).map(|p| (md.as_str().to_string(), serde_json::json!(p))))
                                .collect::<serde_json::Map<String, serde_json::Value>>(),
                        })
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        let v = serde_json::json!({ "providers": providers, "output_formats": output_formats });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    let mut table = comfy_table::Table::new();
    table.load_style(comfy_table::presets::UTF8_FULL_CONDENSED);
    table.set_header(["Model", "Modes", "Default", "Key", "$/page", "Description"]);
    for p in puffinparse_core::PROVIDERS {
        // Self-hosted engines need no key: show "local" instead of a missing-key cross.
        let key = if p.self_hosted() {
            "local"
        } else if key_configured(p.env_var) {
            "✓"
        } else {
            "✗"
        };
        for m in p.models.iter().filter(|m| keep(m)) {
            let q = m.qualified();
            // With a --mode filter one number is enough; otherwise list the price of every mode.
            let price = match mode {
                Some(md) => price_of(&q, md).map(|p| format!("{p:.5}")).unwrap_or_else(|| "?".into()),
                None => m
                    .modes
                    .iter()
                    .map(|md| {
                        let p = price_of(&q, *md).map(|p| format!("{p:.5}")).unwrap_or_else(|| "?".into());
                        format!("{} {p}", md.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            table.add_row([
                q,
                m.modes.iter().map(|md| md.as_str()).collect::<Vec<_>>().join(", "),
                if m.default { "*".into() } else { String::new() },
                key.into(),
                price,
                m.description.into(),
            ]);
        }
    }
    println!("{table}");
    if let Some(m) = mode {
        println!("\nShowing models for mode '{m}'. Drop --mode to see every model.");
    } else {
        println!("\nModes: parse (markdown + blocks), ocr (plain text + boxes), extract (JSON schema).");
    }
    println!(
        "Keys are read from: {}",
        puffinparse_core::PROVIDERS
            .iter()
            .filter(|p| p.key_required())
            .map(|p| p.env_var)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "Self-hosted (no key, $0/page; point them at your install): tesseract (TESSERACT_CMD), \
         docling (DOCLING_BASE_URL), paddleocr (PADDLEOCR_BASE_URL), vllm (VLLM_BASE_URL)"
    );
    println!("Native output formats (--output-format, json only): {}", output_formats.join(" | "));
    Ok(())
}

fn key_configured(env_var: &str) -> bool {
    std::env::var(env_var).map(|v| !v.is_empty()).unwrap_or(false)
}
