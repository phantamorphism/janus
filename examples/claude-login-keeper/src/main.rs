//! `claude-login-keeper serve --config <file>`
//! `claude-login-keeper login --config <file> --credential <ref>`

use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use claude_login_keeper::admin::Admin;
use claude_login_keeper::config::Config;
use claude_login_keeper::keeper::Keeper;
use claude_login_keeper::service::KeeperPlugin;
use tokio::sync::watch;

/// Example Janus plugin: keeps a Claude subscription credential logged in
/// through an isolated `claude auth login`, rotating each new login into
/// the pool.
#[derive(Parser)]
#[command(version)]
struct Args {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the plugin: the gRPC socket, the loopback page, the checks.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Start a login for one credential, print its authorize URL, read the
    /// pasted code from stdin, and wait for the verdict. Uses the running
    /// keeper's page when it answers, else runs the keeper in-process.
    Login {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        credential: String,
    },
}

type Error = claude_login_keeper::Error;

/// How long `login` waits for an in-process keeper to end its flow.
const LOCAL_KEEPER_GRACE: Duration = Duration::from_secs(10);

/// Exit 0: done (or logged in). 1: the login failed or was cancelled.
/// 2: a usage or configuration error, printed as one line.
#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let result = match Args::parse().command {
        Cmd::Serve { config } => match Config::load(&config) {
            Ok(config) => serve(config).await.map(|()| true),
            Err(e) => Err(e.into()),
        },
        Cmd::Login { config, credential } => match Config::load(&config) {
            Ok(config) => login(config, &credential).await,
            Err(e) => Err(e.into()),
        },
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("claude-login-keeper: {e}");
            ExitCode::from(2)
        }
    }
}

/// A stop flag set on SIGTERM or Ctrl-C, or by the caller.
fn stop_signal() -> Result<Arc<watch::Sender<bool>>, Error> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let tx = Arc::new(watch::channel(false).0);
    let signalled = tx.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        signalled.send_replace(true);
    });
    Ok(tx)
}

async fn stopped(mut rx: watch::Receiver<bool>) {
    let _ = rx.wait_for(|stop| *stop).await;
}

/// Start the keeper loop and the page; returns their tasks.
async fn start_keeper(
    config: &Config,
    stop: &watch::Receiver<bool>,
) -> Result<
    (
        Keeper,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ),
    Error,
> {
    let admin = Admin::from_config(config.admin.as_deref(), config.admin_token_file.as_deref())?;
    tracing::info!(event = "config_loaded", admin = %admin.address(), state_dir = %config.state_dir.display(),
        claude = %config.claude_bin.display(), credentials = config.credentials.len(),
        page = %format!("http://{}/", config.http_bind), deny_when_unusable = config.deny_when_unusable);
    let keeper = Keeper::new(config.clone(), admin);
    let listener = tokio::net::TcpListener::bind(config.http_bind).await?;
    let looping = {
        let keeper = keeper.clone();
        let stop = stop.clone();
        tokio::spawn(async move { keeper.run(stopped(stop)).await })
    };
    let page = tokio::spawn(claude_login_keeper::http::serve(
        keeper.clone(),
        listener,
        stopped(stop.clone()),
    ));
    Ok((keeper, looping, page))
}

async fn serve(config: Config) -> Result<(), Error> {
    let stop_tx = stop_signal()?;
    let stop = stop_tx.subscribe();
    let (keeper, looping, page) = start_keeper(&config, &stop).await?;
    let served = claude_login_keeper::serve(
        KeeperPlugin::new(keeper),
        &config.socket,
        config.daemon_uid,
        stopped(stop.clone()),
    )
    .await;
    // The socket is closed (or never opened): stop the loop and the page
    // too. The loop cancels a running flow before it returns.
    stop_tx.send_replace(true);
    let _ = looping.await;
    let _ = page.await;
    served
}

/// The CLI side of a login: through the page of a running keeper, or of
/// one started here.
async fn login(config: Config, credential: &str) -> Result<bool, Error> {
    if config.credential(credential).is_none() {
        return Err(format!(
            "credential {credential:?} is not in the config's [[credential]] list"
        )
        .into());
    }
    let base = format!("http://{}", config.http_bind);
    let http = reqwest::Client::new();
    let stop_tx = stop_signal()?;
    let stop = stop_tx.subscribe();
    let local = match http.get(format!("{base}/status")).send().await {
        Ok(_) => None,
        Err(_) => {
            eprintln!("no keeper answers at {base}; running one in this process");
            Some(start_keeper(&config, &stop).await?)
        }
    };
    let ok = drive_login(&http, &base, credential, &stop).await;
    if let Some((_, looping, page)) = local {
        // The loop cancels its flow, ends the child and removes its pid
        // file before it returns.
        stop_tx.send_replace(true);
        if tokio::time::timeout(LOCAL_KEEPER_GRACE, looping)
            .await
            .is_err()
        {
            eprintln!(
                "the keeper did not stop within {}s",
                LOCAL_KEEPER_GRACE.as_secs()
            );
        }
        let _ = tokio::time::timeout(LOCAL_KEEPER_GRACE, page).await;
    }
    ok
}

/// Start (or attach to) the flow for `credential` through the page, and
/// follow that flow, by its id, to its verdict.
async fn drive_login(
    http: &reqwest::Client,
    base: &str,
    credential: &str,
    stop: &watch::Receiver<bool>,
) -> Result<bool, Error> {
    let mut poll = tokio::time::interval(Duration::from_millis(500));
    let id = loop {
        match start_flow(http, base, credential).await? {
            Some(id) => break id,
            // An automatic attempt for this credential is still checking:
            // it has no flow yet. Ask again.
            None => tokio::select! {
                _ = poll.tick() => {}
                _ = stopped(stop.clone()) => {
                    cancel(http, base, credential).await;
                    return Ok(false);
                }
            },
        }
    };

    // Pasted codes, line by line, from stdin.
    let (codes_tx, mut codes_rx) = tokio::sync::mpsc::channel::<String>(4);
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if codes_tx.blocking_send(line).is_err() {
                break;
            }
        }
    });
    let mut printed = false;
    let mut notice: Option<String> = None;
    let ok = loop {
        tokio::select! {
            _ = poll.tick() => {}
            Some(line) = codes_rx.recv() => {
                if !line.trim().is_empty() {
                    let sent = http.post(format!("{base}/login/code")).header("accept", "application/json")
                        .form(&[("credential", credential), ("code", line.trim())]).send().await;
                    eprintln!("code {}", if sent.is_ok_and(|r| r.status().is_success()) { "submitted" } else { "not accepted" });
                }
                continue;
            }
            _ = stopped(stop.clone()) => {
                cancel(http, base, credential).await;
                break false;
            }
        }
        let status = status(http, base).await?;
        let flows = status["flows"].as_array().cloned().unwrap_or_default();
        let Some(flow) = flows.iter().find(|f| f["id"].as_u64() == Some(id)) else {
            return Err(format!("login flow {id} is no longer listed by the keeper").into());
        };
        let field = |k: &str| flow[k].as_str().map(str::to_owned);
        match flow["state"].as_str().unwrap_or_default() {
            "awaiting_browser" => {
                if !printed {
                    let url = field("auth_url").unwrap_or_default();
                    println!(
                        "Open this URL, sign in, then paste the code it shows here:\n\n  {url}\n"
                    );
                    if let Some(local) = field("local_url") {
                        println!("(From a browser on this host this one completes by itself:\n  {local})\n");
                    }
                    println!("Paste code: ");
                    printed = true;
                }
                if let Some(n) = field("notice") {
                    if notice.as_ref() != Some(&n) {
                        eprintln!("claude: {n}");
                        notice = Some(n);
                    }
                }
            }
            "succeeded" => {
                println!("Logged in; credential {credential} rotated.");
                break true;
            }
            "failed" => {
                println!("Not logged in: {}", field("reason").unwrap_or_default());
                break false;
            }
            "cancelled" => {
                println!("Login cancelled: {}", field("reason").unwrap_or_default());
                break false;
            }
            _ => {}
        }
    };
    Ok(ok)
}

/// `POST /login/start`. The id of the flow this login follows: the new
/// one, or the one already running for `credential`. `None` while an
/// attempt for `credential` runs that has no flow yet.
async fn start_flow(
    http: &reqwest::Client,
    base: &str,
    credential: &str,
) -> Result<Option<u64>, Error> {
    let started = http
        .post(format!("{base}/login/start"))
        .header("accept", "application/json")
        .form(&[("credential", credential)])
        .send()
        .await?;
    let success = started.status().is_success();
    let body: serde_json::Value = started.json().await.unwrap_or_default();
    if success {
        return body["flow_id"]
            .as_u64()
            .map(Some)
            .ok_or_else(|| "the keeper started a flow without an id".into());
    }
    let error = &body["error"];
    if error["code"] == "busy" && error["credential_ref"] == credential {
        return Ok(error["flow_id"].as_u64());
    }
    Err(error["message"]
        .as_str()
        .unwrap_or("the keeper refused the start")
        .to_owned()
        .into())
}

/// `POST /login/cancel` for `credential`.
async fn cancel(http: &reqwest::Client, base: &str, credential: &str) {
    let _ = http
        .post(format!("{base}/login/cancel"))
        .header("accept", "application/json")
        .form(&[("credential", credential)])
        .send()
        .await;
    eprintln!("cancelled");
}

/// `GET /status`, as JSON.
async fn status(http: &reqwest::Client, base: &str) -> Result<serde_json::Value, Error> {
    Ok(http
        .get(format!("{base}/status"))
        .send()
        .await?
        .json()
        .await?)
}
