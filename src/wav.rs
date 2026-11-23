//! Validação de WAV: PCM16, mono ou estéreo, taxa entre 8000 e 96000 Hz
//! (a regra do 16 kHz mono só vale para a gravação de entrada, nunca para
//! a reprodução da voz do Piper).

use std::fmt;

pub const MIN_PLAYBACK_RATE: u32 = 8_000;
pub const MAX_PLAYBACK_RATE: u32 = 96_000;
pub const MAX_WAV_READ_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub data_len: usize,
}

#[derive(Debug)]
pub enum WavError {
    NoHeader(String),
    Unsupported(String),
    TooBig(String),
}

impl fmt::Display for WavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WavError::NoHeader(m) => {
                write!(f, "sem cabeçalho WAV válido (o PCM puro não é aceito): {m}")
            }
            WavError::Unsupported(m) => write!(f, "WAV não suportado: {m}"),
            WavError::TooBig(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for WavError {}

impl WavInfo {
    /// Valida um WAV de gravação de entrada: PCM16, mono, 16 kHz.
    pub fn check_recording(&self) -> Result<(), WavError> {
        if self.sample_rate != 16_000 {
            return Err(WavError::Unsupported(format!(
                "taxa {} Hz; a gravação deve ser 16 kHz",
                self.sample_rate
            )));
        }
        if self.channels != 1 {
            return Err(WavError::Unsupported(format!(
                "{} canais; a gravação deve ser mono",
                self.channels
            )));
        }
        self.check_common("gravação")
    }

    /// Valida um WAV de saída (voz do Piper): PCM16, mono/estéreo,
    /// 8000..=96000 Hz. Não reusa a regra do 16 kHz.
    pub fn check_playback(&self) -> Result<(), WavError> {
        if !(MIN_PLAYBACK_RATE..=MAX_PLAYBACK_RATE).contains(&self.sample_rate) {
            return Err(WavError::Unsupported(format!(
                "taxa {} Hz fora de {}..={} para reprodução",
                self.sample_rate, MIN_PLAYBACK_RATE, MAX_PLAYBACK_RATE
            )));
        }
        self.check_common("reprodução")
    }

    fn check_common(&self, uso: &str) -> Result<(), WavError> {
        if self.bits_per_sample != 16 {
            return Err(WavError::Unsupported(format!(
                "{} bits por amostra; esperado 16",
                self.bits_per_sample
            )));
        }
        if self.channels == 0 || self.channels > 2 {
            return Err(WavError::Unsupported(format!(
                "{} canais; use mono ou estéreo no {uso}",
                self.channels
            )));
        }
        Ok(())
    }

    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.data_len as f64 / (self.sample_rate as f64 * self.channels as f64 * 2.0)
        }
    }
}

fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Analisa um arquivo WAV completo e devolve os metadados do canal.
pub fn parse_wav(bytes: &[u8], max_bytes: usize) -> Result<WavInfo, WavError> {
    if bytes.len() > max_bytes {
        return Err(WavError::TooBig(format!(
            "áudio maior que o teto de {max_bytes} bytes"
        )));
    }
    if bytes.len() < 44 {
        return Err(WavError::NoHeader(format!(
            "{} bytes é menos que um cabeçalho",
            bytes.len()
        )));
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::NoHeader(
            "magic bytes RIFF/WAVE não encontrados".into(),
        ));
    }
    let mut offset = 12usize;
    let mut info: Option<WavInfo> = None;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32le(&bytes[offset + 4..offset + 8]) as usize;
        let body_start = offset + 8;
        if body_start + size > bytes.len() {
            return Err(WavError::NoHeader(format!(
                "chunk {} declara {} bytes que não existem",
                String::from_utf8_lossy(id),
                size
            )));
        }
        if id == b"fmt " {
            if size < 16 {
                return Err(WavError::NoHeader("chunk fmt menor que 16 bytes".into()));
            }
            let body = &bytes[body_start..body_start + size];
            let format = u16le(&body[0..2]);
            let channels = u16le(&body[2..4]);
            let rate = u32le(&body[4..8]);
            let bits = u16le(&body[14..16]);
            if format != 1 {
                return Err(WavError::Unsupported(format!(
                    "formato {format}; só aceitamos PCM (1)"
                )));
            }
            // o chunk 'data' completo ainda não foi posicionado: a contagem
            // final é ajustada abaixo
            info = Some(WavInfo {
                sample_rate: rate,
                channels,
                bits_per_sample: bits,
                data_len: 0,
            });
        } else if id == b"data" {
            let Some(info) = info.as_mut() else {
                return Err(WavError::NoHeader(
                    "chunk data antes do fmt é rejeitado".into(),
                ));
            };
            info.data_len = size;
            if offset + 8 + size > bytes.len() {
                info.data_len = bytes.len() - body_start;
            }
        }
        offset = body_start + size + (size % 2); // chunks são pareados
    }
    match info {
        Some(info) if info.data_len > 0 => Ok(info),
        Some(_) => Err(WavError::NoHeader("sem chunk data não vazio".into())),
        None => Err(WavError::NoHeader("sem chunk fmt".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monta um WAV PCM16 de teste.
    fn make_wav(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data_len = samples.len() * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        let byte_rate = rate * channels as u32 * 2;
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&(channels * 2).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data_len as u32).to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    #[test]
    fn gravação_valida_16k_mono() {
        let wav = make_wav(16_000, 1, &[100_i16; 160]);
        let info = parse_wav(&wav, 1024 * 1024).unwrap();
        info.check_recording().unwrap();
        assert_eq!(info.duration_secs(), 0.01);
    }

    #[test]
    fn playback_22050_mono_aceita() {
        let wav = make_wav(22_050, 1, &[100_i16; 441]);
        let info = parse_wav(&wav, 1024 * 1024).unwrap();
        info.check_playback().unwrap();
    }

    #[test]
    fn playback_estereo_48000_aceita() {
        let wav = make_wav(48_000, 2, &[100_i16; 96]);
        let info = parse_wav(&wav, 1024 * 1024).unwrap();
        info.check_playback().unwrap();
    }

    #[test]
    fn gravação_estereo_rejeitada() {
        let wav = make_wav(16_000, 2, &[1, 2, 3, 4]);
        let info = parse_wav(&wav, 1024 * 1024).unwrap();
        assert!(info.check_recording().is_err());
    }

    #[test]
    fn playback_96001_hz_rejeitada() {
        let wav = make_wav(96_001, 1, &[1, 2, 3, 4]);
        let info = parse_wav(&wav, 1024 * 1024).unwrap();
        assert!(info.check_playback().is_err());
    }

    #[test]
    fn pcm_puro_da_erro_claro() {
        let raw = vec![0u8; 320];
        let err = parse_wav(&raw, 1024 * 1024).unwrap_err();
        assert!(matches!(err, WavError::NoHeader(_)));
        assert!(err.to_string().contains("PCM puro"));
    }

    #[test]
    fn formato_nao_pcm_rejeitado() {
        let mut wav = make_wav(16_000, 1, &[1, 2]);
        wav[20] = 3; // format != 1
        let err = parse_wav(&wav, 1024 * 1024).unwrap_err();
        assert!(matches!(err, WavError::Unsupported(_)));
    }

    #[test]
    fn arquivo_pequeno_rejeitado() {
        assert!(parse_wav(&[0u8; 10], 1024).is_err());
    }
}
