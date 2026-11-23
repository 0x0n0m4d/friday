# Progresso (docs/PROGRESS.md)

## Estado

Projeto completo em uma passada (o brief §8 pedia fase por fase; o usuário pediu tudo de uma vez,
então P0–P6 foram construídas em sequência dentro da mesma sessão). Verificação atual:
`./scripts/verify.sh` passa inteiro.

## Por fase

- **P0 Fundação** — concluída. Crates: lib + 2 bins. `cli`, `config` (TOML com validação),
  `logger` (JSONL com retenção/redação), `state` (0700/0600, flock), `scripts/verify.sh`.
- **P1 Cliente ACP** — concluída. WebSocket direto no endpoint `/acp` do gateway (RFC 6455 em
  `src/acp/ws.rs`, sem dependências novas), quadros de linha lógicos com teto de 1 MiB,
  `initialize`, `session/new`, streaming, permissões, `session/cancel`, SESSION_BUSY, load/resume
  expirada, entrada malformada. Fio leitor → canal; escritas serializadas.
  (Transporte por bridge existiu na revisão 1 e foi removido a pedido do usuário.)
- **P2 TUI de conversa** — concluída. ratatui/crossterm; conversa com rolagem, campo de digitação,
  linha de status, modal de aprovação, sanitização de tudo que vem do agente, restauração do
  terminal (inclusive em pânico). Digitação funciona sempre que a tecla de voz não está pressionada.
- **P3 Gravação e Whisper** — concluída. Gravador via argv com `{file}`, grupo próprio, prazo e
  validação estrita de WAV (mono/16 kHz/PCM16); STT por comando com placeholders ou servidor em
  loopback; temporários privados apagados após uso.
- **P4 Push-to-talk** — concluída. `KeySource` com implementação evdev (hold) e fallback toggle
  (f8 por padrão); `--find-key`; apenas o código configurado é examinado; regra para tecla presa.
- **P5 Piper** — concluída. Filtro de fala (markdown/tabelas/URLs/caminhos; identificadores intactos),
  fila por frases, teto de bytes imposto durante a leitura, interrupção imediata.
- **P6 Integração e endurecimento** — concluída. Testes de fluxo do aplicativo contra o simulador
  (fila de voz, expiração, reconexão, permissão), mensagens de erro em português, README + docs.

## Bugs encontrados e corrigidos (cada um com regressão em tests/)

0. O `cargo test` travava com os testes todos verdes: os testes soltavam o filho simulador sem
   matar e o simulador (loop de `accept`) segura o stdout herdado para sempre, então o cargo espera
   um EOF que nunca chega. Corrigido nos próprios testes: guard `Simulador` que encerra e reaproveita
   o filho ao sair (`tests/acp_client.rs`, `tests/app_flow.rs`).
1. `App::begin_new_session` mantinha a fase `Initializing` — cada resposta de `session/new` era
   tratada como `initialize` e reenviada sem fim (tempestade de reconexão). Regressão:
   `regressao_begin_new_session_muda_a_fase`.
2. O status voltava a `livre` por trás do modal de permissão quando o fim do turno chegava junto com
   o pedido — o modal agora mantém o status de aprovação até ser respondido.
3. Configuração do simulador por variáveis de ambiente corria contra testes paralelos; o simulador
   passou a ser configurado por argumentos.
4. `--check-config` validava o TOML mas não rodava a validação completa; agora roda `config::validate`.
5. Falha do evdev era engolida em `EvdevKeySource::run` (`drop(e)`) e prompt sem sessão era engolido
   em `send_prompt`; ambos agora seguem D13 (`KeySourceEvent::Failed` + `[diag]` com `--verbose`).
   Regressão: `src/ptt.rs::dispositivo_inexistente_emite_failed` e
   `tests/app_flow.rs::verbose_mostra_prompt_sem_sessao_e_padrao_esconde`.

## O que NÃO está coberto (e por quê)

- Evdev real, áudio real, teclado real e daemon real: proibidos nos testes; cobertos pelos cenários
  manuais de `docs/TESTING.md` (§12 do brief).
- A API do `whisper-server` em loopback depende da instalação do usuário (item VERIFY em
  `docs/DECISIONS.md`); o caminho por comando é o primário e está testado.
- Aparência da TUI em emuladores reais (cores, redimensionamento, colagem): os testes de unidade não
  inspecionam desenho; ensaio manual recomendado.
