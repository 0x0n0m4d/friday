//! Remoção de caracteres de controle e sequências de escape de todo texto
//! que não vem do usuário (saída do modelo é tratada como não confiável).

/// Remove sequências de escape ANSI (CSI, OSC, DCS e C1) e caracteres de
/// controle C0/C1, preservando `\n` e convertendo `\t` em espaço.
pub fn strip_escapes_and_control(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push(' '),
            '\r' => {}
            '\x1b' => {
                // Sequência de escape: CSI "\x1b[...final(0x40-0x7e)",
                // OSC/DCS/APC/PM "\x1b] ... BEL" ou "\x1b] ... ST(\x1b\\)"
                // ou sequência curta "\x1b<um byte>"
                match chars.peek().copied() {
                    Some('[') => {
                        chars.next();
                        while let Some(&c2) = chars.peek() {
                            chars.next();
                            if ('\u{40}'..='\u{7e}').contains(&c2) {
                                break;
                            }
                        }
                    }
                    Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                        chars.next();
                        let mut prev = ' ';
                        for c2 in chars.by_ref() {
                            if c2 == '\x07' {
                                break;
                            }
                            if prev == '\x1b' && c2 == '\\' {
                                break;
                            }
                            prev = c2;
                        }
                    }
                    Some(_) => {
                        // sequência de dois bytes com final em 0x30..0x7e
                        if let Some(c2) = chars.next() {
                            if !('\u{30}'..='\u{7e}').contains(&c2) {
                                // não é terminador conhecido; descarta os dois
                            }
                        }
                    }
                    None => {}
                }
            }
            c => {
                let code = c as u32;
                if code < 0x20 || code == 0x7f || (0x80..=0x9f).contains(&code) {
                    // controle: descarta
                } else {
                    out.push(c);
                }
            }
        }
    }
    out
}

/// Limita o comprimento de um texto, caracteres (não bytes) contados.
pub fn cap_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out = String::new();
    let mut bytes = 0usize;
    for c in text.chars() {
        if bytes + c.len_utf8() > text.len() || out.chars().count() >= max_chars {
            break;
        }
        out.push(c);
        bytes += c.len_utf8();
    }
    out
}

/// "…" indica truncamento para o usuário.
pub fn cap_chars_ellipsis(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut = cap_chars(text, max_chars.saturating_sub(1));
    format!("{cut}\u{2026}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_csi_e_osc() {
        let input = "ok\u{1b}[31mvermelho\u{1b}[0m fim \u{1b}]0;título x\u{7}fim";
        assert_eq!(strip_escapes_and_control(input), "okvermelho fim fim");
    }

    #[test]
    fn remove_st_osc() {
        let input = "a\u{1b}]2;t;ítulo\u{1b}\\b";
        assert_eq!(strip_escapes_and_control(input), "ab");
    }

    #[test]
    fn preserva_menos_e_tab() {
        let input = "linha1\nlinha2\tfim\u{7}";
        assert_eq!(strip_escapes_and_control(input), "linha1\nlinha2 fim");
    }

    #[test]
    fn remove_caracteres_c1_e_null() {
        let input = "a\u{0}b\u{9c}c\u{7f}";
        assert_eq!(strip_escapes_and_control(input), "abc");
    }

    #[test]
    fn limita_por_caracteres_nao_bytes() {
        let s = "áéíóú"; // 10 bytes, 5 chars
        assert_eq!(cap_chars_ellipsis(s, 3).chars().count(), 3);
        assert!(cap_chars_ellipsis(s, 10) == s);
    }
}
