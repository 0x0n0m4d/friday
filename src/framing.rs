//! Enquadramento de linhas (JSON-RPC delimitado por `\n`) com limite de tamanho.

use std::fmt;
use std::io::{self, BufRead, Write};

/// Limite por linha (1 MiB) para evitar consumo desmedido de memória.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    LineTooLong,
    NotUtf8,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "erro de E/S: {e}"),
            FrameError::LineTooLong => {
                write!(f, "linha maior que o limite de {MAX_LINE_BYTES} bytes")
            }
            FrameError::NotUtf8 => write!(f, "linha não é UTF-8 válido"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Grava uma linha completa (sem `\n` no fim) acrescentando o delimitador.
pub fn write_frame<W: Write>(w: &mut W, line: &str) -> io::Result<()> {
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}

/// Lê uma linha até `\n`. `Ok(None)` indica fim de fluxo.
/// Falha se a linha exceder `MAX_LINE_BYTES` ou não for UTF-8.
pub fn read_frame<R: BufRead + Unpin>(r: &mut R) -> Result<Option<String>, FrameError> {
    let mut out = Vec::new();
    loop {
        let avail = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                if out.is_empty() {
                    return Err(FrameError::Io(e));
                }
                return Err(FrameError::Io(e));
            }
        };
        if avail.is_empty() {
            if out.is_empty() {
                return Ok(None);
            }
            // fluxo terminou sem \n final: processa o que restou
            break;
        }
        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
            out.extend_from_slice(&avail[..pos]);
            let consumed = pos + 1;
            r.consume(consumed);
            if out.len() > MAX_LINE_BYTES {
                return Err(FrameError::LineTooLong);
            }
            return match String::from_utf8(out) {
                Ok(s) => Ok(Some(s)),
                Err(_) => Err(FrameError::NotUtf8),
            };
        }
        if out.len() + avail.len() > MAX_LINE_BYTES {
            return Err(FrameError::LineTooLong);
        }
        out.extend_from_slice(avail);
        let len = avail.len();
        r.consume(len);
    }
    if out.len() > MAX_LINE_BYTES {
        return Err(FrameError::LineTooLong);
    }
    match String::from_utf8(out) {
        Ok(s) => Ok(Some(s)),
        Err(_) => Err(FrameError::NotUtf8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn le_uma_linha_e_para_no_fim() {
        let mut r = Cursor::new(b"{\"a\":1}\n{\"b\":2}\n".to_vec());
        assert_eq!(read_frame(&mut r).unwrap(), Some("{\"a\":1}".into()));
        assert_eq!(read_frame(&mut r).unwrap(), Some("{\"b\":2}".into()));
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn linha_sem_quebra_final_e_entregue_no_fim_do_fluxo() {
        let mut r = Cursor::new(b"teste".to_vec());
        assert_eq!(read_frame(&mut r).unwrap(), Some("teste".into()));
    }

    #[test]
    fn linha_muito_grande_falha() {
        let mut r = Cursor::new(vec![b'x'; MAX_LINE_BYTES + 1]);
        assert!(matches!(read_frame(&mut r), Err(FrameError::LineTooLong)));
    }

    #[test]
    fn escreve_com_delimitador() {
        let mut dst = Vec::new();
        write_frame(&mut dst, "hello").unwrap();
        assert_eq!(dst, b"hello\n");
    }
}
