# Decisões do projeto (docs/DECISIONS.md)

Decisões, motivos/evidências e itens `VERIFY:` (incertos que dependem da instalação real do usuário).

## D1. Nome do binário/crate (brief §11.1)

`sexta-feira` (crate `sexta-feira`, biblioteca `sexta_feira`), binário auxiliar de testes
`sexta-mock-agente`.

## D2. Formato de configuração (brief §11.2)

**TOML** com `serde` + `deny_unknown_fields` (chave desconhecida é erro de configuração, não
silêncio). Motivo: legível, crate maduro (`toml 0.8`), e o brief sugere TOML.

## D3. ACP: cliente escrito à mão, sem o crate `agent-client-protocol`

O brief permite explicitamente um cliente pequeno para o subconjunto usado. O crate oficial já teve
várias quebras de versão maior; aqui usamos `serde_json` puro com um leitor de quadros (uma linha =
uma mensagem, teto de 1 MiB). Evidência: brief §3 (quadros de exemplo) e a nota sobre o crate.

## D4. Transporte: WebSocket direto no endpoint `/acp` do gateway (brief §11.4)

`[acp].url` é um `ws://` para o gateway do daemon (canal `channel-acp-server`), autenticado com
`Authorization: Bearer` quando `[acp].token_env` aponta uma variável de ambiente. Motivos:

- a pedido do usuário: nenhuma bridge, nenhum binário extra do ZeroClaw instalado na máquina;
- o gateway do ZeroClaw expõe ACP sobre WebSocket em `/acp` (moldura JSON-RPC 2.0, uma mensagem por
  frame de texto). O cliente NÃO anuncia `Sec-WebSocket-Protocol`: o servidor também aceita token
  via esse header, e anunciar o subprotocolo fez o daemon 0.9.0 real aceitar o upgrade e nunca
  responder ao `initialize` (sonda manual sem o header recebe resposta — evidência de 2026-11);
- o token valor fica no ambiente do usuário, nunca no arquivo de configuração; o servidor aceita
  também `Sec-WebSocket-Protocol: bearer.<token>` e `?token=`, mas o cliente usa só o cabeçalho;
- uma porta fixa em código é proibida — a porta vem da `[acp].url`;
- restrições assumidas: só `ws://`, só host que resolve para loopback (daemon local), sem TLS
  (`wss://` é recusado), sem redirecionamento HTTP; `Sec-WebSocket-Accept` não é validado (VERIFY
  abaixo). WebSocket é implementado em `src/acp/ws.rs` com a biblioteca padrão (sem dependências
  novas), conforme D3.

Fontes consultadas em 2026-11-21: documentação oficial `docs.zeroclaw.com` (canal ACP) e o
`crates/zeroclaw-gateway/src/acp.rs` no repositório do ZeroClaw (master). O transporte por bridge
(`ui_exec`/similar) existiu na v0.1 do projeto e foi removido a pedido do usuário; `deny_unknown_fields`
faz TOMLs antigos com `bridge_command` falharem com mensagem listando os campos válidos.

## D5. Push-to-talk: evdev no modo `hold`, `toggle` como fallback (brief §11.3)

- Modo padrão `hold`: um fio próprio abre **apenas** o dispositivo configurado (`/dev/input/event*`)
  e só processa eventos do **código configurado**; auto-repetição (valor 2) é ignorada; tecla presa
  é liberada automaticamente após `max_record_secs`.
- Modo `toggle` (fallback para máquinas sem acesso a `/dev/input`): tecla da TUI liga/desliga a
  escuta; padrão `f8` (`fallback_key`), configurável.
- Descoberta: `sexta-feira --find-key [dispositivo]` lê UM aperto de tecla, imprime dispositivo +
  código + sugestão de TOML. Erros de permissão mencionam o grupo `input`.
- Nenhum teste toca dispositivo real: a fronteira é o traço `KeySource` (Press/Release).

## D6. Rascunho digitado é preservado (brief §11.5)

Ao apertar a tecla de voz, o texto digitado é congelado (`input_frozen_draft`) e devolvido ao soltar
(ou ao falhar a gravação). O envio da transcrição é automático, sem confirmação (brief §1/§5.3).
O brief pedia confirmação do usuário: deixar como decisão adotada e validar no cenário manual 4.

## D7. Ocupado: no máximo UM aviso de voz em fila (brief §11.6)

Se um turno está em andamento quando a transcrição chega, a mensagem de voz fica em fila (uma
única; a seguinte é descartada com aviso) e é enviada quando o turno termina. Texto digitado
durante um turno não é enfileirado — o usuário é avisado para esperar ou cancelar com Esc.
Testado em `tests/app_flow.rs::voz_enquanto_ocupado_fica_na_fila_e_vai_depois`.

## D8. Teclas da TUI (brief §11.7)

- `Enter` envia; `Esc` cancela o turno (se `[acp].esc_cancels`) ou responde "cancelled" ao modal;
- `Ctrl+C` sai; `PgUp`/`PgDn` rola; setas navegam no modal;
- o mute da voz NÃO existe no cliente (recurso removido por decisão do usuário: mutar é
  papel do microfone/SO; letras `m`/`M` digitam normalmente);
- a tecla de voz vem do evdev (`hold`) ou do `fallback_key` (`toggle`);
- aprovação é SEMPRE por teclado; nunca por voz; nunca automática (brief §5.2).

## D9. TUI/CLI (brief §11.8): `ratatui 0.29` + `crossterm 0.28` + `clap 4`

- `ratatui 0.29`: `default-features = false`, recurso `crossterm` (backend único).
- `crossterm 0.28`: recursos `events` e `bracketed-paste` (colagem chega como um único evento e é
  sanitizada como o resto do texto).
- `clap 4`: sem recursos padrão; só `std`, `help`, `usage`, `error-context`, `derive`.
- Demais: `serde`+`derive`, `serde_json`, `toml 0.8`, `libc` (setpgid/kill/flock), `unicode-width`
  (cursor com caracteres largos); dev: `tempfile 3`.
- A TUI restaura o terminal inclusive em pânico (hook próprio).

## D10. Áudio: regras de gravação e de reprodução são DIFERENTES

Gravação: WAV PCM16, mono, 16 kHz — validação estrita (`wav::check_recording`), pois é o que o
Whisper consome. Reprodução: PCM16 mono OU estéreo, taxa sã entre 8.000 e 96.000 Hz
(`wav::check_playback`) — motivada pela voz do usuário (mono 22050 Hz, lição do protótipo, brief
§5.6). PCM puro sem cabeçalho dá erro claro. O teto de bytes do áudio do piper é imposto **durante**
a leitura, não depois.

## D11. Sessão e estado

`sessionId` por apelido em `${XDG_STATE_HOME:-~/.local/state}/sexta-feira/sessao-<alias>.json`
(0600, gravação atômica por renomeio). No boot: `session/load` e, em falha, `session/new` (VERIFY:
os docs citam também `session/resume`). Expiração (erro de sessão desconhecida/expirada) → nova
sessão + aviso + reenvio da última pergunta — no máximo UMA vez (trava anti-loop de 2026-11-23:
quando o daemon recusou também o reenvio como "sessão expirada", o ciclo de recriar-a-sessão-e-
reenviar virava loop infinito de repetições no chat; hoje o cliente desiste do reenvio automático
e deixa a sessão nova pronta para o próximo envio manual). Queda da conexão → `Disconnected` + recuo exponencial
limitado. Instância única por `flock` (`sexta-feira.lock`); segunda instância sai com código 3.
`--new-session` apaga a sessão salva e começa nova.

**Recarga (2026-11-23, regra definitiva):** ao retomar uma sessão salva, o daemon reenvia o histórico
completo como `session/update` (chunks de usuário e de agente). A sexta-feira trata esse fluxo como
**contexto puro**: reconstrói a tela com a sessão inteira e SÓ isso — não fala nada no Piper, não
reenvia nenhuma mensagem, não existe opção de "repetir" histórico. Implementação: a partir do envio
de `session/load` o cliente começa a descartar/reexibir a tela e arma `historico_silencioso`; a
trava só se solta quando o USUÁRIO envia uma mensagem própria (`send_prompt`), e é aí que a voz
volta. Motivação: antes disso, cada recarga/reconexão tocava a sessão inteira no Piper e parecia um
loop de repetição sem fim. `user_message_chunk` (linhas do usuário no histórico) passou a ser exibido
em `Author::User`, sem afeto colateral.

**Reconexão (2026-11-23, regra definitiva):** a tela de conversa é um **log individual — um
mostrador de mensagens**, não influenciado por nada além do próprio fluxo. O PRIMEIRO handshake
de cada execução marca o limite (`ja_carregou`): qualquer `session/load` depois dele é RECONEXÃO.
Na reconexão (queda em pleno uso e retomada automática), o replay completo que o daemon devolve
(`agent_message_chunk`, `user_message_chunk`, `tool_call`/`tool_call_update`) é **engolido**
(`engolir_replay`): nada entra na tela, nada fala, nada se duplica — a tela já é o log oficial da
sessão, e a conversa segue da última mensagem exibida. O usuário é avisado ("a tela segue de onde
parou") e a voz volta só no próximo prompt. Na abertura (primeira carga), a regra anterior vale:
replay reconstrói a tela inteira, mudo e sem reenvio.

## D12. Logs

JSONL diário em `state/journal/sexta-feira.<dia>.jsonl`; retenção por dias (padrão 30) e teto por
arquivo; redação de segredos (cachas hexadecimais ≥ 16 caracteres) **antes** de gravar. Eventos:
conexão, sessão, press/release da tecla do PTT, fim de turno (`stopReason`), erros com motivo,
mais `diag` (veja D13). Nunca com conteúdo: sem prompts, transcrições ou texto do modelo no log.

## D13. Diagnóstico: erros essenciais vs. de praxe (`--verbose`)

Dois níveis de visibilidade, decisão do usuário após depurar conexão viva e silêncio da aplicação:

- **Essencial** (sempre na tela, sem opção): tudo que impede o uso normal — conexão perdida,
  gravador/ transcrição/voz falhando, modal de permissão, falta de comando configurado.
- **De praxe** (journal sempre; tela só com `--verbose`/`-v`): o que o cliente tolera em silêncio —
  falha da fonte da tecla (`KeySourceEvent::Failed`, fim do `drop(e)` em `ptt.rs`), prompt guardado
  por falta de sessão em `send_prompt`, handshake sem resposta (aviso periódico em `on_tick`,
  sem timeout automático — comportamento não muda), notificações/respostas desconhecidas ou órfãs,
  fios auxiliares que não iniciaram, `Esc` sem efeito, token ausente, persistência de sessão falha.

Na tela, itens verbosos são `Author::Diag`, rótulo `[diag]`, cor amarela. O journal registra tudo
igualmente nos dois modos (eventos `diag`/`erro_ptt`); `--verbose` só muda a TELA. Conteúdo
proibido continua proibido (D12): nenhum prompt, transcrição, texto do modelo ou segredo.

---

## Respostas às perguntas em aberto do brief (§2)

1. **Como a saída do Whisper chega ao agente pela ACP?** Como TEXTO em `session/prompt`
   (`params.prompt`), mesmo que o agente anuncie a capacidade `audio` — ela é apenas registrada
   (brief §5.4 manda não depender dela). Fluxo: tecla press → gravador (argv) → validação do WAV →
   Whisper (argv ou servidor loopback) → sanitização/teto → `session/prompt`; resposta volta como
   `session/update` (`agent_message_chunk`) e termina com `stopReason`. Evidência: quadros do brief
   §3; testes `tests/acp_client.rs::handshake_e_prompt_com_streaming` e `tests/app_flow.rs`.
   Confirmar com o cenário manual 2/4 contra o daemon real.
2. **Esperar a tecla sem bloquear a digitação?** Duas fontes de entrada concorrentes: um fio no
   evdev (consulta de 50 ms) e um fio no teclado da TUI (crossterm); ambos empurram num único canal
   consumido pelo laço da TUI com `recv_timeout(25 ms)`. Nenhum fio bloqueia o outro. Evidência:
   `src/ptt.rs` (`EvdevKeySource`), `src/tui.rs::run`.
3. **Como a tecla "vence" o teclado e não chega ao texto?** A leitura evdev é passiva: a tecla
   configurada AINDA chega ao stdin do terminal. Defesas: (a) sugerir teclas que não digitam nada
   (Right Alt/Ctrl, Scroll Lock, Pause); (b) no modo `hold` a digitação é congelada enquanto a
   tecla está pressionada; (c) no modo `toggle` a tecla de escuta é filtrada no parse do teclado e
   nunca entra no campo de texto. VERIFY: comportamento varia por emulador de terminal; validar no
   cenário manual 4.
4. **O que dispara o envio ao soltar, e e se o agente estiver ocupado?** Soltar para o gravador →
   transcrição → envio automático. Turno em andamento ⇒ fila de no máximo uma mensagem de voz,
   enviada quando o turno termina; a seguinte é descartada com aviso. Transcrição vazia ou só de
   marcador de silêncio não é enviada: mensagem no chat.
5. **Como o Piper recebe a saída, e o que é falado?** Só o texto de `agent_message_chunk` é falado;
   a fala sai por frases completas (buffer procura `.`/`!`/`?`/`…`), filtrada por
   `tts::prepare_for_speech`: cercas de código, tabelas, URLs e caminhos longos são removidos;
   `**`/`__`, cabeçalhos e bullets são removidos; identificadores como `snake_case` são mantidos
   intactos. Chamadas de ferramenta são mostradas, nunca faladas. Fila com ordem, teto de bytes e
   tempo, interrompida na hora pela tecla de voz ou Esc.
6. **Relação com as sessões de rotinas/canais do daemon?** Este cliente mantém a PRÓPRIA sessão por
   agente (`session/new` com `agentAlias`, `sessionId` persistido e retomado com
   `session/load`/`session/resume`). As sessões de rotinas/canais pertencem ao daemon; por isso a
   expiração (3600 s padrão segundo os docs) é tratada como rotina: nova sessão + aviso + reenvio.
   VERIFY: semântica exata de `session/load` vs `session/resume` e de `sessionTimeoutSecs` no
   `_meta` do daemon real (cenário manual 6).

---

## D14. Chamada de ferramentas — REMOVIDO o lembrete de turno (revoga o D14 original)

**Histórico:** o D14 original (2026-11-23) criou `[agent].tool_reminder` — um prefixo de regra por
turno que ensinava ao modelo o dialeto textual (etiqueta tool_call com JSON de "name" e
"arguments", parâmetros como chave=valor) para contornar modelos gratuitos que respondiam bash em
markdown (`parsed_tool_calls: 0`).

**Revogação (2026-11-25):** com a engenharia reversa do ZeroClaw v0.9.0, a abordagem foi rejeitada
pelo dono do projeto:

- `agent.rs` (dispatcher): o `tool_dispatcher` do runtime profile decide o modo em conjunto com a
  capacidade do provider. Dispatcher `native` envia as `tools` no corpo da requisição HTTP e espera
  `tool_calls` estruturados de volta; o modo texto/xml escreve as instruções de protocolo no system prompt
  (via bloco de protocolo que o cliente NÃO deve reimplementar) e depende de parser textual.
- Enviar lembrete textual pelo cliente é duplicar (de forma pior) o que o modo xml já faz, e não
  resolve modelos `:free` que declaram `tools` mas não respeitam nativamente.
- Princípio do dono: "A ideia não é utilizar conteúdos externos para testar funcionalidades
  internas. O próprio código precisa se resolver." O lembrete era exatamente isso (conteúdo externo
  injetado no prompt).

**Decisão:** REMOVIDO por completo do lado do cliente — nada de `[agent].tool_reminder`,
`tool_reminder_text`, `DEFAULT_TOOL_REMINDER` nem anexo de payload em `send_prompt`; o prompt do
usuário vai intacto ao daemon. O campo de fluxo correto é o daemon:

```toml
[runtime_profiles.perfil-do-sextafeira]
tool_dispatcher = "native"
```

Verificação: `tools_count` maior que zero e `native_tool_calls` 1 ou mais no
`~/.zeroclaw/data/state/runtime-trace.jsonl`; modal de permissão aparece na sexta-feira. Se o modelo
`:free` não devolve tool calls nativos, a solução é trocar de modelo — não voltou ao lembrete.

**Migração:** quem tinha `[agent]` no `~/.config/friday/config.toml` deve REMOVER a seção inteira
(a config agora rejeita chaves desconhecidas e a carga falha com erro apontando para a linha).

**Notas de manutenção:** os testes `lembrete_*` foram removidos; o repo não contém mais referência
a lembretes de ferramenta.

---

## Itens VERIFY (incertos que dependem da instalação do usuário)

| Item | Onde | Por quê |
|---|---|---|
| API exata do `whisper-server` (POST do WAV, texto na resposta) | `src/stt.rs::transcribe_server` | cada instalação expõe endpoints diferentes; o caminho por comando (`[whisper].command`) é o suportado de primeira; `server_url` exige loopback |
| semântica `session/load` vs `session/resume` | `src/app.rs` (handshake) | brief §3 cita os dois como extensões ZeroClaw; o cliente usa `load` e cai para `new` em falha |
| forma exata dos parâmetros de `session/request_permission` (`toolCall.title`, `options[].optionId`) | `src/app.rs::open_permission_modal` | baseado nos docs e no simulador; validável só contra o daemon real |
| marcadores de silêncio impressos pelo Whisper instalado | `[whisper].silence_markers` | confirmar o que a sua versão imprime e ajustar a lista |
| capacidade `audio` do agente | registro no log + aviso no chat | o cliente nunca depende dela |
| exatidão do `sessionTimeoutSecs: 3600` | D11 / pergunta 6 | valor vem dos docs, não do daemon em execução |
| quebra de linha ao fim de cada frame de texto enviado | `src/acp/ws.rs` | o cliente envia `\n` ao fim de cada frame para ser compatível com leitura por linha; validável só contra o daemon real |
| não validação do `Sec-WebSocket-Accept` no handshake | `src/acp/ws.rs` | base64 SHA-1 exigiria dependência ou implementação própria de SHA-1; o handshake já exige `HTTP/1.1 101` + `Upgrade: websocket` num socket de loopback |
| nome da variável de token (`ZEROCLAW_ACP_TOKEN`) | `README.md`, `[acp].token_env` | o NOME é escolha do usuário; o projeto só lê a variável que ele configurar |


## Bug relevante registrado (regressão coberta por teste)

`begin_new_session` não trocava a fase do handshake de `Initializing` para `CreatingSession`; toda
resposta de `session/new` era tratada como resultado de `initialize` e reenviada sem fim (tempestade
de reconexão). Corrigido em `src/app.rs`; regressão:
`tests/app_flow.rs::regressao_begin_new_session_muda_a_fase` (falha antes do conserto, passa depois).
