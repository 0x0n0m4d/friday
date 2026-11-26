//! Mock do Piper para as provas do pipeline de voz (tests/tts_pipeline.rs).
//!
//! Lê linhas da entrada padrão e, para cada uma, escreve um WAV PCM16 válido
//! na saída padrão — comportamento do Piper real em `[piper].output =
//! "stdout"`. O corpo sai em duas doses com pausa entre elas: prova que o
//! coletor da sexta-feira é streaming de verdade, não bloqueio até o fim.
//!
//! Flags:
//!   --bytes N          corpo de áudio fixo, além do cabeçalho
//!   --por-char N       corpo cresce N bytes por caractere da linha (vira
//!                      assinatura do fragmento no log do reprodutor)
//!   --delay-ms N       pausa entre as doses (estressa o pipeline)
//!   --falhar-prefixo S linha que começa com S não produz áudio e mata o
//!                      processo com erro (Piper comprometido)

use std::io::{BufRead, Write};
use std::time::Duration;

fn lota(flag: &str, fallback: u64) -> u64 {
    let alvo = format!("--{flag}");
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == &alvo)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

fn texto(flag: &str) -> Option<String> {
    let alvo = format!("--{flag}");
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == &alvo)
        .and_then(|i| args.get(i + 1).cloned())
}

/// Cabeçalho RIFF/WAVE PCM16 mono válido (o mesmo formato dos testes de wav).
fn cabecalho_wav(data_len: usize, rate: u32, channels: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    out
}

fn main() {
    let bytes = lota("bytes", 1200) as usize;
    let por_char = lota("por-char", 0) as usize;
    let delay = lota("delay-ms", 0);
    let falhar = texto("falhar-prefixo").unwrap_or_default();

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let stdin = std::io::stdin();
    for linha in stdin.lock().lines() {
        let Ok(linha) = linha else { break };
        if linha.trim().is_empty() {
            continue;
        }
        if !falhar.is_empty() && linha.starts_with(&falhar) {
            // Piper comprometido: nada sai e o processo morre com erro.
            // O pipeline tem que pular o fragmento e continuar vivo.
            std::process::exit(2);
        }
        let data_len = bytes + por_char * linha.chars().count();
        let _ = out.write_all(&cabecalho_wav(data_len, 22_050, 1));
        let _ = out.flush();
        // corpo em duas doses com pausa: obriga o coletor a ler em fluxo
        for dose in [data_len / 2, data_len - data_len / 2] {
            let _ = out.write_all(&vec![0u8; dose]);
            let _ = out.flush();
            if delay > 0 {
                std::thread::sleep(Duration::from_millis(delay));
            }
        }
    }
}
