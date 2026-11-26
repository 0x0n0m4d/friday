//! Provas do pipeline de voz contra processos-mock REAIS (sexta-mock-piper
//! e sexta-mock-player): ordem garantida, falha isolada, interrupção limpa.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use sexta_feira::tts::{Speaker, SpeakerConfig, TtsEvent};

fn dir_tmp() -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "sexta-teste-voz-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn cfg_voz(dir: &Path, log: &Path, piper_delay_ms: u64, player_delay_ms: u64) -> SpeakerConfig {
    SpeakerConfig {
        piper_command: vec![
            env!("CARGO_BIN_EXE_sexta-mock-piper").into(),
            "--bytes".into(),
            "1200".into(),
            "--por-char".into(),
            "8".into(),
            "--delay-ms".into(),
            piper_delay_ms.to_string(),
        ],
        piper_output: "stdout".into(),
        player_command: vec![
            env!("CARGO_BIN_EXE_sexta-mock-player").into(),
            "--log".into(),
            log.display().to_string(),
            "--delay-ms".into(),
            player_delay_ms.to_string(),
            "{file}".into(),
        ],
        timeout_secs: 20,
        max_wav_bytes: 512 * 1024,
        tmp_dir: dir.to_path_buf(),
    }
}

/// Frase longa (acima de FRAG_MINIMO_CHARS): chega ao Piper sozinha — a
/// assinatura de bytes (por-char) permite auditar ordem e conteúdo no log.
fn frase(tag: &str) -> String {
    format!(
        "{tag} — esta frase é longa o suficiente para exceder o tamanho mínimo de fragmento do falante e chega ao Piper sozinha, sem unir vizinhas na fila."
    )
}

fn bytes_esperados(f: &str) -> usize {
    44 + 1200 + 8 * f.chars().count()
}

/// Espera o primeiro evento que casa com `casa` (ou desiste no teto).
fn esperar(rx: &Receiver<TtsEvent>, teto: Duration, casa: impl Fn(&TtsEvent) -> bool) -> bool {
    let fim = Instant::now() + teto;
    loop {
        let restante = fim.saturating_duration_since(Instant::now());
        if restante.is_zero() {
            return false;
        }
        match rx.recv_timeout(restante) {
            Ok(e) if casa(&e) => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

#[test]
fn as_frases_chegam_ao_reprodutor_na_ordem_e_inteiras() {
    let dir = dir_tmp();
    let log = dir.join("ordem.log");
    let (tx, rx) = mpsc::channel();
    let sp = Speaker::new(cfg_voz(&dir, &log, 5, 5), tx);

    let frases: Vec<String> = ["um", "dois", "tres"].iter().map(|t| frase(t)).collect();
    for f in &frases {
        sp.speak(f.clone());
    }

    for _ in 0..3 {
        assert!(esperar(&rx, Duration::from_secs(20), |e| {
            matches!(e, TtsEvent::Finished(Ok(())))
        }));
    }

    // o reprodutor viu exatamente 3 WAVs, em ordem, com tamanho distinto por
    // frase (assinatura por-char): nada falou fora de vez nem truncado
    let registro = std::fs::read_to_string(&log).unwrap();
    let linhas: Vec<String> = registro.lines().map(|l| l.to_string()).collect();
    let esperadas: Vec<String> = frases
        .iter()
        .map(|f| format!("bytes={}", bytes_esperados(f)))
        .collect();
    assert_eq!(linhas, esperadas);
    sp.shutdown();
}

#[test]
fn fragmento_falho_e_pulado_e_o_falante_continua_vivo() {
    let dir = dir_tmp();
    let log = dir.join("falha.log");
    let (tx, rx) = mpsc::channel();
    let mut cfg = cfg_voz(&dir, &log, 5, 5);
    cfg.piper_command.push("--falhar-prefixo".into());
    cfg.piper_command.push("EXPLODE".into());
    let sp = Speaker::new(cfg, tx);

    // fila: um fragmento bom; um falho — que, pela coalescência, arrasta
    // a frase boa seguinte para dentro dele (fragmento curto une ao próximo
    // até o mínimo); um bom depois de tudo — o falante tem que sobreviver.
    sp.speak(frase("antes"));
    sp.speak("EXPLODE: pista de falha do piper mock".to_string());

    assert!(
        esperar(&rx, Duration::from_secs(20), |e| {
            matches!(e, TtsEvent::Finished(Ok(())))
        }),
        "o primeiro fragmento deveria tocar"
    );
    assert!(
        esperar(&rx, Duration::from_secs(20), |e| {
            matches!(e, TtsEvent::Finished(Err(_)))
        }),
        "a falha não é silenciosa: tem que vir evento de erro"
    );

    // o falante tem que continuar vivo depois da falha
    sp.speak(frase("final"));
    assert!(
        esperar(&rx, Duration::from_secs(20), |e| {
            matches!(e, TtsEvent::Finished(Ok(())))
        }),
        "o falante morreu junto com o fragmento falho"
    );

    let registro = std::fs::read_to_string(&log).unwrap();
    let linhas: Vec<String> = registro.lines().map(|l| l.to_string()).collect();
    let esperadas: Vec<String> = [frase("antes"), frase("final")]
        .iter()
        .map(|f| format!("bytes={}", bytes_esperados(f)))
        .collect();
    assert_eq!(linhas, esperadas);
    sp.shutdown();
}

#[test]
fn interromper_descarta_o_resto_sem_quebrar_o_falante() {
    let dir = dir_tmp();
    let log = dir.join("corte.log");
    let (tx, rx) = mpsc::channel();
    // piper lento e player LENTO: o corte precisa apanhar o áudio em pleno voo
    let sp = Speaker::new(cfg_voz(&dir, &log, 150, 500), tx);

    sp.speak(frase("primeira"));
    sp.speak(frase("segunda"));
    assert!(
        esperar(&rx, Duration::from_secs(20), |e| {
            matches!(e, TtsEvent::Started)
        }),
        "o primeiro áudio precisa começar a tocar"
    );
    sp.interrupt();
    assert!(
        esperar(&rx, Duration::from_secs(20), |e| {
            matches!(e, TtsEvent::Interrupted)
        }),
        "interromper tem que anunciar Interrupted"
    );

    // fala nova usa a rodada nova e toca até o fim; nada da rodada antiga
    // (nem a primeira em voo, nem a segunda em fila) chega ao reprodutor
    sp.speak(frase("nova"));
    assert!(
        esperar(&rx, Duration::from_secs(20), |e| {
            matches!(e, TtsEvent::Finished(Ok(())))
        }),
        "a fala da rodada nova precisa tocar até o fim"
    );

    let registro = std::fs::read_to_string(&log).unwrap();
    let linhas: Vec<String> = registro.lines().map(|l| l.to_string()).collect();
    // só a frase "nova" tocou — o corte realmente matou o áudio em voo e o
    // fragmento enfileirado não apareceu
    assert_eq!(
        linhas,
        vec![format!("bytes={}", bytes_esperados(&frase("nova")))],
        "a rodada antiga não pode alcançar o reprodutor"
    );
    sp.shutdown();
}
