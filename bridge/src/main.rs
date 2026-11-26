//! Bens Chat ↔ Matrix appservice. See /home/mgs/matrix/PLAN.md.

mod benschat;
mod cli;
mod config;
mod db;
mod incoming;
mod matrix;
mod outgoing;
mod shim;

use std::sync::Arc;
use std::time::Duration;

use bens_chat_shared::{Bot, Message};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct State {
    pub cfg: config::Config,
    pub db: db::Db,
    pub mx: matrix::Matrix,
    pub bc: benschat::BensChat,
}

async fn state() -> Result<Arc<State>, Error> {
    let cfg = config::Config::from_env()?;
    let db = db::Db::open(&cfg.database_url).await?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    Ok(Arc::new(State {
        mx: matrix::Matrix::new(&cfg, http.clone()),
        bc: benschat::BensChat::new(&cfg, http),
        db,
        cfg,
    }))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bc_bridge=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    if matches!(a.first(), Some(&("-h" | "--help" | "help"))) {
        println!("{}", cli::USAGE);
        return;
    }
    let st = match state().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("startup failed: {e}");
            std::process::exit(1);
        }
    };
    let res = match a.as_slice() {
        [] | ["run"] => run(st).await,
        ["link", net, user] => cli::link(&st, net, user, None).await,
        ["link", net, user, flow] => cli::link(&st, net, user, Some(flow)).await,
        ["rooms", user] => cli::rooms(&st, user).await,
        ["create-chat", room, user, net] => cli::create_chat(&st, room, user, net).await,
        ["map", chat, room, user, net] => cli::map(&st, chat, room, user, net).await,
        ["unmap", chat] => cli::unmap(&st, chat).await,
        ["chats"] => cli::chats(&st).await,
        _ => {
            eprintln!("{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(st: Arc<State>) -> Result<(), Error> {
    if st.cfg.bc_user != "t0" {
        tracing::warn!(
            "BENS_CHAT_USER={} but the shared crate's Bot socket still joins as t0; \
             outgoing messages only work for chats t0 is in until that is made configurable",
            st.cfg.bc_user
        );
    }
    // the appservice sender user, and every known puppet
    st.mx.ensure_registered("benschat_bot").await.ok();
    for (_, mxid) in st.db.users().await? {
        if let Some(lp) = mxid.strip_prefix('@').and_then(|s| s.split(':').next())
            && let Err(e) = st.mx.ensure_registered(lp).await
        {
            tracing::warn!("registering {mxid}: {e}");
        }
    }
    outgoing::preload(&st).await;

    if let Ok(target) = std::env::var("LOCALHOST_SHIM_TARGET")
        && !target.is_empty()
    {
        shim::start(target).await?;
    }

    // Path A: the shared crate's blocking socket client on its own thread;
    // the callback only enqueues (ordering + never blocks the socket).
    let (tx_a, rx_a) = tokio::sync::mpsc::unbounded_channel::<Message>();
    std::thread::Builder::new()
        .name("benschat-socket".into())
        .spawn(move || {
            Bot::no_responce(move |m| {
                let _ = tx_a.send(m);
            })
        })?;
    tokio::spawn(outgoing::run(rx_a, st.clone()));

    // Path B: appservice transactions → queue → worker
    let (tx_b, rx_b) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(incoming::run(rx_b, st.clone()));

    let listener = tokio::net::TcpListener::bind(st.cfg.listen).await?;
    tracing::info!(
        listen = %st.cfg.listen,
        bens_chat_user = st.cfg.bc_user,
        "bridge running"
    );
    axum::serve(listener, incoming::router(st, tx_b))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

/// Ctrl-C, or SIGTERM from `docker stop`.
async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}
