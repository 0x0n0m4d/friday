# Testes (docs/TESTING.md)

## Regime

- Verificação única: `./scripts/verify.sh` — na ordem: `cargo fmt --check`, `cargo check
  --all-targets`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo build --release`.
- Nenhum teste toca: o `HOME` real (todos usam temporários), microfone, caixas de som, dispositivos
  de entrada, teclas reais, rede além de loopback iniciado pelo teste, ou o daemon do usuário.
- O agente remoto é sempre o simulador (`src/bin/sexta-mock-agente`), acionado por argumentos
  (`--busy`, `--permission`, `--malformed`, `--load-bad`, `--prompt-unknown-session`); programas
  falsos de gravador/Whisper/piper/reprodutor são gerados pelos próprios testes.

## Cobertura automatizada (resumo por módulo)

| Área | Suíte | O que cobre |
|---|---|---|
| sanitização | unidade `src/sanitize.rs` | escapes CSI/OSC/ST, controles, `\n` preservado, teto de caracteres com elipse |
| quadros JSON-RPC | unidade `src/framing.rs` + `src/acp/client.rs` | linhas válidas/lixo, teto de 1 MiB, UTF-8, sem pânico em entrada malformada |
| tipos ACP | unidade `src/acp/types.rs` | `session/update` (chunk em múltiplos formatos, tool_call), erros e classes (busy, expirada) |
| cliente ACP | `tests/acp_client.rs` (7) | handshake, streaming em partes, SESSION_BUSY, linha malformada → `Closed`, permissão ida-e-volta, sessão desconhecida em load/resume, morte do filho sem órfãos |
| aplicativo | `tests/app_flow.rs` (8) | handshake + persistência da sessão, retomada no segundo boot, fila de voz pós-turno, expiração recria e reenvia, modal de permissão (abrir/navegar/responder/cancelar), reconexão com recuo, regressão da fase do handshake |
| WAV | unidade `src/wav.rs` | caminhador de chunks, gravação 16 kHz mono PCM16, reprodução 8–96 kHz mono/estéreo, PCM puro recusado |
| gravador | unidade `src/recorder.rs` | subprocesso com `{file}`, grupo próprio, prazo, validação, remoção do temporário |
| transcrição | unidade `src/stt.rs` | substituição de placeholders, teto de saída/prazo, marcadores de silêncio, servidor loopback obrigatório |
| voz | unidade `src/tts.rs` | filtragem para fala (cercas, tabelas, URLs, caminhos, ênfase, cabeçalhos), frases, interrupção, teto de bytes |
| teclado evdev | unidade `src/ptt.rs` | só o código configurado, press/release/repetição, liberação automática de tecla presa |
| configuração | unidade `src/config.rs` | TOML válido, campo desconhecido, placeholders, `.en` recusado, permissões do arquivo, limites |
| estado/log | unidade `src/state.rs` + `src/logger.rs` | 0700/0600, sessão por apelido, trava de instância (flock), JSONL, retenção, redação de segredos |

Contagem atual: 66 testes de unidade + 19 de integração = **85**.

## Cenários manuais para o usuário (brief §12) — SEMPRE no seu turno

Estes nunca são executados pelo agente: tocam daemon real, áudio real e teclado real. Faça na ordem.

### 1. Gateway e capacidades

1. Confirme que o daemon está em pé: `zeroclaw --version` e o status do serviço (lado do daemon).
2. Confirme que o gateway do daemon está exposto no endpoint `/acp` (canal `channel-acp-server`)
   e anote a porta. Não há bridge para subir: o cliente conecta direto em
   `ws://127.0.0.1:<porta>/acp`.
3. Se o gateway exigir token, exporte-o (mesmo nome configurado em `[acp].token_env`):
   `export ZEROCLAW_ACP_TOKEN=<seu token>`.
4. Rode o cliente com `--check-config` para validar a `[acp].url` antes de abrir a TUI.
5. Num ensaio rápido sem o cliente, um `websocat ws://127.0.0.1:<porta>/acp` e uma única linha
   `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}` deve devolver `protocolVersion` `1`,
   o `_meta.zeroclaw` e a `capabilities.promptCapabilities.audio`. Se `audio` vier verdadeiro,
   apenas registre: o cliente segue mandando texto mesmo assim.

### 2. Conversa digitada contra o daemon real

1. Use um TOML com a `[acp].url` real (modelo na seção 4 do `README.md`) e rode:
   `sexta-feira --agent <alias> --new-session`
2. Digite uma pergunta e pressione Enter. Esperado: status `aguardando` e a resposta chegando em
   blocos; ao fim, status volta a `livre`.
3. Feche com Ctrl+C e rode de novo SEM `--new-session`: esperado o aviso “Sessão anterior retomada”.

### 3. Aprovação pelo teclado

1. Pergunte algo que acione uma ferramenta com autonomia `supervised`.
2. Esperado: modal central com as opções; setas movem; Enter responde; a ferramenta procede ou é
   cancelada; a resposta final chega normalmente.
3. Repita respondendo com Esc: esperado o modal responder “cancelled” e o turno terminar sem a ação.

### 4. Push-to-talk completo (hold)

1. Rode `sexta-feira --find-key`, aperte a tecla escolhida e complete o TOML com device + code.
2. Verifique as permissões: usuário no grupo `input` e acesso de leitura a `/dev/input/event*`.
3. Com o programa aberto, digite um rascunho no campo de texto e SEGURE a tecla de voz:
   esperado digitação congelada (rascunho preservado, devolvido ao soltar), status `ouvindo`.
4. Fale uma frase e solte: esperado status `transcrevendo` → a mensagem vai sozinha para o agente
   (sem confirmação) e a resposta chega e é falada.
5. Enquanto a fala acontece, segure a tecla de voz: a fala para imediatamente.
6. Solte a tecla sem falar nada: esperado “Não detectei fala nessa gravação” e nada enviado.
7. Confirme que, com a tecla de voz SOLTA, a digitação normal funciona (inclusive a tecla escolhida,
   se ela digita algo no seu emulador de terminal — documente o comportamento observado).

### 5. Interrupção da fala

1. Com uma resposta longa sendo falada, segure a tecla de voz (ou pressione Esc).
2. Esperado: fala cortada no ato; status sai de `falando`; a fila de áudio esvazia.

### 6. Queda do daemon e expiração de sessão

1. Durante uma conversa, reinicie o serviço do ZeroClaw.
2. Esperado: status `desconectado`, aviso no chat, tentativas com recuo crescente e retorno
   automático; a sessão pode ser reaberta (nova, com aviso, se a antiga expirou).
3. Deixe uma sessão ociosa acima do limite informado pelo daemon (`sessionTimeoutSecs`, padrão
   documentado de 3600 s) e mande uma mensagem: esperado aviso de sessão expirada + nova sessão +
   reenvio automático da pergunta.

### 7. Piper com a sua voz e o reprodutor

1. Configure `[piper].command` com o binário nativo e a voz `.onnx` do usuário, e o `[player]`.
2. Dispare uma resposta curta. Esperado: áudio tocado na taxa do piper (a voz do usuário é mono
   22050 Hz PCM16 — o validador aceita).
3. Force uma falha (por exemplo, caminho errado da voz) e confirme que o problema aparece na linha
   de status e no log — nunca silenciosamente — e que o texto do chat continua legível.
