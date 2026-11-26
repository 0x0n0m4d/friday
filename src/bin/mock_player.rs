//! Mock do reprodutor para as provas do pipeline de voz.
//!
//! Lê os bytes (WAV) da fonte indicada (o `{file}` do comando vira
//! `/dev/stdin`) até o fim, anota o tamanho recebido em ordem no log
//! (`--log CAMINHO`) e sai com sucesso. `--delay-ms` simula uma duração de
//! reprodução, deixando o pipeline antecipar a síntese do próximo fragmento.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::time::Duration;

fn valor(flag: &str) -> Option<String> {
    let alvo = format!("--{flag}");
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == &alvo)
        .and_then(|i| args.get(i + 1).cloned())
}

fn lota(flag: &str, fallback: u64) -> u64 {
    valor(flag).and_then(|v| v.parse().ok()).unwrap_or(fallback)
}

fn main() {
    let log_path = valor("log").expect("mock do reprodutor precisa de --log");
    let delay = lota("delay-ms", 0);
    // o arquivo é o último argumento (no comando real, o placeholder {file})
    // next_back em vez de last: não percorre o iterador inteiro (clippy).
    let fonte = std::env::args()
        .next_back()
        .expect("mock do reprodutor precisa do arquivo como último argumento");
    let mut buf = Vec::new();
    let mut erro = None;
    if fonte == "/dev/stdin" {
        if let Err(e) = std::io::stdin().lock().read_to_end(&mut buf) {
            erro = Some(format!("leitura da entrada falhou: {e}"));
        }
    } else if let Err(e) = std::fs::File::open(&fonte).and_then(|mut f| f.read_to_end(&mut buf)) {
        erro = Some(format!("leitura de {fonte} falhou: {e}"));
    }
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .expect("abrir o log do reprodutor");
    if let Some(e) = erro {
        eprintln!("{e}");
        let _ = writeln!(log, "erro={e}");
        std::process::exit(1);
    }
    // duração simulada ANTES de anotar: o log marca o fim da reprodução
    if delay > 0 {
        std::thread::sleep(Duration::from_millis(delay));
    }
    writeln!(log, "bytes={}", buf.len()).expect("escrever no log");
}
