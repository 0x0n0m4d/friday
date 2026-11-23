//! Binário principal: CLI → configuração → TUI (ou --find-key/--check-config).

use std::sync::{mpsc, Arc};

use clap::Parser;
use sexta_feira::app::App;
use sexta_feira::cli::Args;
use sexta_feira::config;
use sexta_feira::logger::Logger;
use sexta_feira::ptt;
use sexta_feira::state::{self, StateDir};
use sexta_feira::tui;

fn main() {
    let args = Args::parse();

    if let Some(device) = args.find_key.clone() {
        let pinned = if device.trim().is_empty() {
            None
        } else {
            Some(device)
        };
        match ptt::find_key(pinned, 2_500) {
            Ok(info) => {
                println!("{info}");
                println!("coloque no TOML (seção [push_to_talk]):");
                println!("  device = \"...\" e code = ... correspondentes acima");
            }
            Err(e) => {
                eprintln!("sexta-feira: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let Some(agent) = args.agent.clone() else {
        eprintln!("sexta-feira: informe --agent <apelido> (apelido do agente no ZeroClaw)");
        eprintln!("use --help para ajuda; --find-key descobre a tecla do push-to-talk");
        std::process::exit(2);
    };

    let path = match args.config.clone() {
        Some(p) => std::path::PathBuf::from(p),
        None => match config::default_config_path() {
            Some(p) => p,
            None => {
                eprintln!("sexta-feira: sem HOME/XDG_CONFIG_HOME e sem --config");
                std::process::exit(2);
            }
        },
    };
    let cfg = match config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sexta-feira: {}", e);
            std::process::exit(2);
        }
    };

    if args.check_config {
        match config::validate(&cfg) {
            Ok(()) => println!("ok: configuração válida ({})", path.display()),
            Err(e) => {
                eprintln!("sexta-feira: {e}");
                std::process::exit(2);
            }
        }
        return;
    }

    let state_dir = match cfg.paths.state_dir.clone() {
        Some(s) => StateDir::open(std::path::PathBuf::from(s)),
        None => match state::default_state_dir() {
            Some(d) => StateDir::open(d),
            None => {
                eprintln!("sexta-feira: sem HOME/XDG_STATE_HOME para o diretório de estado");
                std::process::exit(2);
            }
        },
    };
    let state_dir = match state_dir {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sexta-feira: {e}");
            std::process::exit(2);
        }
    };
    if args.new_session {
        state_dir.clear_session(&agent);
    }
    let _instance_lock = match state_dir.try_lock() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("sexta-feira: {e}");
            std::process::exit(3);
        }
    };

    let log = Arc::new(Logger::open(&state_dir.journal_dir()));
    log.prune(u64::from(cfg.log.retain_days), cfg.log.max_file_bytes);

    let (events_tx, events_rx) = mpsc::channel();
    let mut app = App::new(cfg, agent, Some(state_dir), log, events_tx);
    app.set_verbose(args.verbose);
    tui::run(app, events_rx);
}
