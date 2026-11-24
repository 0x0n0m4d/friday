# Sexta-feira

Cliente ACP com TUI e **push-to-talk** (segure uma tecla, fale, solte) para conversar com um agente
do **daemon ZeroClaw** já em execução na sua máquina.

Este programa **não** roda o ZeroClaw, não roda nenhum modelo e não grava nada por conta própria:
ele conversa com o daemon pela **ACP (Agent Client Protocol, JSON-RPC 2.0 sobre stdio)**, grava áudio
com o **comando que você configurar**, transcreve com o seu **Whisper** local e fala a resposta com o
seu **Piper** + reprodutor. Nenhum comando externo, tecla ou porta está fixo no código: tudo vem do
arquivo de configuração.

- Binário: `sexta-feira` (mais `sexta-mock-agente`, um agente ACP de mentira para testes e ensaio).
- Escrito em Rust 1.100, sem `sh -c` em lugar nenhum, sem segredos no arquivo de configuração.

---

## 1. Pré-requisitos (o que VOCÊ precisa ter/instalar)

| Componente | Para quê | Observações |
|---|---|---|
| Daemon **ZeroClaw** em execução no host, com o **gateway** exposto (canal `channel-acp-server`, endpoint `/acp`) | fornece a ACP deste cliente | este projeto não inicia nem supervisiona o daemon |
| Rust **1.100+** (via rustup, com `clippy` e `rustfmt`) | compilar | `rustup component add clippy rustfmt` |
| **Acesso ao grupo `input`** | ler eventos do teclado em `/dev/input/event*` para o push-to-talk | `sudo usermod -aG input $USER` e refazer a sessão |
| **Whisper local** (ex.: `whisper-cli` com modelo ggml) | transcrever a gravação | use um modelo **multilíngue** (nunca um modelo `.en`); ou um servidor local |
| **Piper nativo** (binário) + voz `.onnx` | falar as respostas | a sua voz produz WAV mono 22050 Hz PCM16 — o validador aceita isso |
| **Reprodutor de áudio** (ex.: `paplay`, `ffplay`, `aplay`) | tocar o WAV do Piper | o cliente empurra o WAV pela entrada padrão |

**Não é preciso instalar bridge nem binário extra do ZeroClaw**: o cliente fala WebSocket direto com
o gateway do daemon em `ws://<loopback>/acp`.

A tecla de voz é lida **direto do dispositivo evdev** (modo padrão `hold`), então nada depende de
recursos do emulador de terminal. Sem acesso a `/dev/input`, use o modo `toggle` (fallback por
teclado, ver seção 4.1).

---

## 2. Compilar

```bash
cargo build --release
```

Script de verificação completo (formatação, cheque, testes, clippy e build):

```bash
./scripts/verify.sh
```

Os binários ficam em `target/release/sexta-feira` e `target/release/sexta-mock-agente`.

---

## 3. Descobrir a tecla do push-to-talk

```bash
./target/release/sexta-feira --find-key
# ou, para fixar um dispositivo:
./target/release/sexta-feira --find-key /dev/input/event4
```

Pressione **uma vez** a tecla que quer usar. O programa imprime o dispositivo, o código evdev e o
Trecho de TOML pronto. Nenhum outro código de tecla é lido, registrado ou impresso. Sugestões de
teclas que não digitam nada: Right Alt / Right Ctrl (códigos 100/97), Scroll Lock (70), Pause (119).

---

## 4. Configuração

Arquivo padrão: `~/.config/sexta-feira/sexta-feira.toml` (ou `$XDG_CONFIG_HOME/sexta-feira/sexta-feira.toml`).
Caminho alternativo: `--config /caminho/arquivo.toml`.

Exemplo completo comentado (checar depois com `--check-config`):

```toml
# ---- push-to-talk -------------------------------------------------
[push_to_talk]
# "hold"   = segurar a tecla para falar (lê evdev; precisa de device + code)
# "toggle" = apertar uma vez para começar, de novo para parar (teclado da TUI)
mode = "hold"
# O número de event4 PODE mudar ao reiniciar. Prefira o link estável do udev:
#   ls -l /dev/input/by-id/ | grep event   → use esse caminho em vez do eventN
device = "/dev/input/event4"   # saída do --find-key (ou de /dev/input/by-id/)
code   = 100                   # código evdev da tecla (100 = Right Alt)
min_press_ms   = 300           # apertos mais curtos que isso são ignorados
max_record_secs = 120          # teto de segurança da gravação
# fallback_key = "f8"          # tecla usada no modo "toggle" (padrão: f8)

# ---- gravação -----------------------------------------------------
[recording]
# Obrigatório no modo hold. Contrato: gravar WAV PCM16, MONO, 16 kHz.
# {file} = caminho do WAV temporário (obrigatório).
command = ["/usr/bin/arecord", "-D", "default", "-r", "16000", "-c", "1",
           "-f", "S16_LE", "{file}"]

# ---- transcrição (Whisper) ----------------------------------------
[whisper]
# Forma 1: comando local. Placeholders: {audio} (obrigatório), {model}, {lang}.
command = ["/usr/local/bin/whisper-cli", "-m", "{model}", "-l", "{lang}",
           "-f", "{audio}", "--no-prints"]
model = "/opt/whisper/ggml-base-q5_1.bin"   # multilíngue; NUNCA um modelo ".en"
language = "pt"
# Forma 2 (alternativa ao command): servidor whisper local em loopback.
# server_url = "http://127.0.0.1:8123/transcribe"
timeout_secs = 120
max_chars = 4000            # teto do texto transcrito
# Marcações de silêncio que o seu Whisper imprime são descartadas (padrões: [BLANK_AUDIO], [SILENCE], [NO SPEECH], [SILÊNCIO])
# silence_markers = ["[BLANK_AUDIO]"]

# ---- voz (Piper) --------------------------------------------------
[piper]
# Contrato: texto entra na entrada padrão; WAV sai na saída padrão (output = "stdout")
# OU o piper grava no arquivo {file} (output = "file").
# ATENÇÃO: NÃO use --output-raw — ele produz PCM sem cabeçalho RIFF e o cliente
# rejeita todo o áudio. O exemplo abaixo usa arquivo (opção mais à prova de
# versões): confira a flag exata do seu Piper com `piper --help`.
command = ["/usr/local/bin/piper", "--model", "/opt/voices/pt-br-faber-medium.onnx", "--output_file", "{file}"]
output = "file"
timeout_secs = 60
max_wav_bytes = 67108864
enabled = true              # false desliga a voz (só texto)

# ---- reprodutor ---------------------------------------------------
[player]
# {file} é sempre substituído por "/dev/stdin" quando o piper está em "stdout",
# então o reprodutor lê o WAV da entrada padrão do cliente. Em "file", {file}
# vira o caminho do WAV temporário.
command = ["/usr/bin/paplay", "{file}"]

# ---- conexão com o daemon ZeroClaw --------------------------------
[acp]
# Único transporte suportado: WebSocket direto no endpoint /acp do gateway.
# Só ws:// e só loopback (daemon local); wss:// é recusado de propósito.
url = "ws://127.0.0.1:42617/acp"
# Opcional: agente padrão escopo-conexão (equivale a ?agent=<alias>), sem espaços, ? ou &.
agent = "dev"
# Opcional: NOME da variável de ambiente que contém o token do gateway.
# Nunca o valor do token aqui — o cliente lê o ambiente na hora de conectar.
token_env = "ZEROCLAW_ACP_TOKEN"
handshake_timeout_secs = 10
prompt_max_chars = 8000
esc_cancels = true          # Esc cancela o turno em andamento

# ---- comportamento do agente --------------------------------------
[agent]
# Modelos gratuitos (e alguns locais) às vezes respondem com comandos bash em
# blocos de markdown em vez de chamar ferramentas de verdade — aí nenhum
# modal de permissão aparece, porque nenhuma ferramenta foi pedida (no
# runtime-trace do daemon: parsed_tool_calls: 0). Este lembrete vai prefixado
# a CADA turno para impor o comportamento. Desligue depois de trocar para um
# modelo que respeita tool calls, se quiser economizar tokens.
tool_reminder = true
# tool_reminder_text = "..."   # sobrepõe o texto padrão do lembrete (uma linha)

# ---- limites e logs ------------------------------------------------
[limits]
reconnect_backoff_min_ms = 1000   # reconexão após queda da conexão
reconnect_backoff_max_ms = 30000
max_chat_items = 500              # itens de conversa em memória

[log]
retain_days = 30                  # retenção do journal
max_file_bytes = 10485760         # teto por arquivo; ao passar, abre outro

[paths]
# state_dir = "/var/lib/sexta-feira"   # sobrepõe o diretório de estado
```

Validar sem abrir a TUI:

```bash
./target/release/sexta-feira --agent casa --check-config
```

Validações aplicadas, entre outras: chave TOML desconhecida é erro (`deny_unknown_fields`), `{file}`
obrigatório na gravação e no reprodutor, `{audio}` obrigatório no Whisper, idioma/`.en` rejeitado,
`[piper].output` só `stdout`/`file`, e permissões do arquivo TOML não podem conceder escrita a
grupo/outros (use `0600` ou `0640`).

### 4.1 Modo `toggle` (sem acesso a /dev/input)

```toml
[push_to_talk]
mode = "toggle"
fallback_key = "f8"   # F8 liga e desliga a escuta
```

O restante do fluxo (gravar → transcrever → enviar → falar) é idêntico.

### 4.2 Lembrete de ferramenta (`[agent]`)

O modal de permissão só aparece quando o modelo emite uma **tool call de verdade** que o ZeroClaw
reconhece (formato `<tool_call>` XML ou JSON de tool call). Se o modelo responde com um bloco de código bash em markdown fingindo que executou, o ZeroClaw não tem nada para aprovar: `parsed_tool_calls: 0`
e nada acontece. A API da OpenRouter confirma que muitos modelos `:free` **declaram** suporte a
`tools` (verifique em `openrouter.ai/api/v1/models/<id>/endpoints`: `supported_parameters` deve conter
`tools`), mas mesmo assim erram o formato na prática — o lembrete por turno existe exatamente para
empurrar esses modelos para o caminho certo.

- `tool_reminder = true` (padrão): cada prompt sai prefixado com uma linha de regra (ver
  `DEFAULT_TOOL_REMINDER` em `src/config.rs`). O lembrete NÃO aparece no chat nem conta para o
  reenvio — só vai no payload para o daemon.
- `tool_reminder_text`: troque para usar a sua própria instrução (por exemplo em inglês, se o seu
  agente opera em inglês).
- `tool_reminder = false`: desliga (recomendado só para modelos que já chamam ferramentas bem).

Complementos no lado do **ZeroClaw** (sem rebuild da sexta-feira): um provider `custom` cai no modo
prompt-guided por padrão — se o endpoint é compatível com OpenAI e aceita `tools`, ponha
`native_tools = true` no provider para usar chamadas nativas; e/ou reforce a persona do agente com a
mesma regra. Trocar para um modelo gratuito que responde bem a tool calls (procure
`supported_parameters=tools` na OpenRouter) reduz a dependência do lembrete.

### 4.3 Recarga e reconexão de sessão (o que acontece ao abrir de novo)

A tela de conversa é **um log individual: um mostrador de mensagens, e só isso**. Ela não é
influenciada pelo modelo nem pela conexão — quem decide o que entra nela é o fluxo abaixo.

**Abertura (primeira carga da execução):** ao abrir a sexta-feira com uma sessão salva, o daemon
reenvia o histórico completo e o cliente **mostra a sessão inteira na tela — e ponto final, só
isso**:
- nada é falado no Piper (o histórico é mudo);
- nenhuma mensagem é reenviada ao agente (não existe opção de "repetir" uma mensagem);
- a voz volta **somente** quando VOCÊ manda uma mensagem nova (voz ou teclado).

**Reconexão (queda em pleno uso):** se a conexão cai e o cliente retoma a sessão automaticamente,
**o replay do histórico é descartado**. A tela já mostra a sessão inteira — ela é o log oficial —
então nada é reexibido, nada é falado e nada se duplica: a conversa continua a partir da última
mensagem que já estava na tela. Você é avisado de que a sessão foi retomada; a voz volta no
próximo prompt seu.

Se a recarga não for a que você quer, use `--new-session` para começar do zero.

### 4.4 O que NÃO configurar aqui

Qualquer segredo. Este projeto não tem chave nenhuma embutida; o token do gateway vem do ambiente,
quando você quiser: aponte o **nome** da variável em `[acp].token_env` (por exemplo
`ZEROCLAW_ACP_TOKEN`) e exporte o valor no seu shell. Se o gateway não exigir token, deixe
`token_env` de fora.

---

## 5. Rodar

```bash
./target/release/sexta-feira --agent <alias-do-agente>            # sessão salva é retomada
./target/release/sexta-feira --agent <alias> --new-session        # ignora a sessão salva
./target/release/sexta-feira --agent <alias> --config ~/outra.toml
./target/release/sexta-feira --agent <alias> --verbose            # diagnostico na tela
```

### 5.1 Diagnóstico (`--verbose`)

Os erros **essenciais** — conexão perdida, gravador, transcrição, voz, modal de permissão — sempre
aparecem na tela, em qualquer modo. Além deles existem erros e caminhos **de praxe** que o cliente
tolera em silêncio para não interromper o uso: falha ao abrir/ler o dispositivo da tecla,
mensagem guardada por falta de sessão, `initialize`/`session/new` sem resposta, notificações ou
respostas desconhecidas do agente, fios auxiliares que não iniciaram, `Esc` sem efeito no estado
atual, token ausente no ambiente. Com `--verbose` (ou `-v`) tudo isso aparece na tela durante o uso,
em linhas amarelas marcadas `[diag]`.

Com ou sem `--verbose`, **tudo** é registrado no journal
(`journal/sexta-feira.<dia>.jsonl`, eventos `diag` e `erro_ptt`) — a opção só muda a TELA.
Diagnóstico nunca imprime prompt, transcrição, texto do modelo ou segredos.

Se o cliente parecer congelado ou a tecla de voz não responder, rode com `--verbose`: o primeiro
`[diag]` aponta o elo quebrado — WebSocket aberto? `initialize` enviado? sessão criada? tecla
pressionada capturada?

Teclas da TUI:

- Digitar e **Enter** envia; **Esc** cancela o turno (configurável) responde "cancelled" no modal de
  permissão; **Ctrl+C** sai; **PgUp/PgDn** rola a conversa.
- **Segure a tecla de voz** (modo hold): a digitação congela, a fala em curso é interrompida na hora,
  grava enquanto pressionada; ao soltar, transcreve e **envia automaticamente** — sem confirmação.
  A resposta do agente chega em streaming e é falada em frases, com Markdown removido.
- Modal de permissão (`session/request_permission`): abre no centro, sobe/desce com setas,
  **Enter** confirma a opção, **Esc** responde "cancelled". Aprovação **nunca** acontece por voz.

Estado e logs (tudo privado, `0700`/`0600`):

```
${XDG_STATE_HOME:-~/.local/state}/sexta-feira/
├── sessao-<alias>.json      # sessionId por agente (0600, gravação atômica)
├── sexta-feira.lock         # trava: uma instância por vez (flock; segunda dá erro e sai com código 3)
├── tmp/                     # WAV temporários de gravação e do piper (0700), apagados após uso
└── journal/sexta-feira.<dia>.jsonl   # log JSONL, retenção por dias
```

O log registra eventos e motivos (`conectou`, `ptt press/release`, `turno stop`, erros) — **nunca**
prompts, transcrições, texto do modelo ou segredos (valores tipo hexadecimal longos são redigidos
antes de entrar no log).

---

## 6. Testes e ensaio sem daemon real

O projeto inclui um **agente ACP simulado** (`sexta-mock-agente`) que fala o subconjunto do
protocolo usado pelo cliente — versões, `session/new`, streaming em duas partes, pedido de
permissão, SESSION_BUSY, sessão desconhecida e linha malformada. Os testes automatizados usam esse
simulador e programas falsos para gravador/Whisper/Piper/reprodutor; **nenhum teste** toca áudio
real, dispositivos reais, teclado real, rede além de loopback ou o seu daemon.

Para ensaiar manualmente contra o simulador, suba ele WebSocket e aponte a `[acp].url` para ele:

```console
$ target/debug/sexta-mock-agente --port 0
PORT=42617
```

```toml
[acp]
url = "ws://127.0.0.1:42617/acp"
# variações para ensaiar caminhos de erro: --busy, --malformed, --load-bad, --prompt-unknown-session,
# --require-token <segredo> (com token_env apontando para uma variável com o mesmo valor)
```

Os sete cenários manuais com o daemon real estão em `docs/TESTING.md`.

---

## 7. Segurança e privacidade

- Comandos externos são **argv**, nunca shell: não existe `sh -c` no projeto.
- Filhos externos (gravador, whisper, piper, reprodutor) nascem em **grupo de processos próprio** e
  morrem com o cliente (SIGTERM → SIGKILL); nada fica órfão.
- O push-to-talk lê o dispositivo evdev e **olha apenas para o código configurado**; qualquer outro
  evento é descartado sem registrar nada.
- O grupo `input` dá leitura a **todos** os teclados — privilégio mais amplo do que o cliente
  precisa. Se preferir restringir, crie um grupo dedicado e uma regra udev por dispositivo
  (descubra o nome do teclado com `udevadm info -a -n /dev/input/event4`):

  ```udev
  # /etc/udev/rules.d/85-sexta-ptt.rules
  SUBSYSTEM=="input", ATTRS{name}=="*<nome do seu teclado>*", \
    KERNEL=="event*", MODE="0640", GROUP="sexta-ptt"
  ```

  Aí basta `sudo groupadd sexta-ptt && sudo usermod -aG sexta-ptt $USER`, refazer a sessão e
  **sem** precisar do grupo `input`. Com regra própria, o cliente lê só o teclado escolhido em
  `[push_to_talk].device`.
- `wss://`/TLS é um **aprimoramento futuro deliberadamente não implementado**: hoje o cliente só
  aceita daemon local em loopback.
- Texto do modelo é tratado como **não confiável**: sequências de escape/controle são removidas
  antes de desenhar a TUI, antes de falar e antes de reenviar.
- A única rede é o **WebSocket** para o gateway do daemon: só `ws://`, só endereço que resolve para
  loopback (`127.0.0.1` e semelhantes), sem TLS e sem redirecionamento. `wss://` e hosts fora do
  loopback são **recusados**. O token (se existir) vai em `Authorization: Bearer` e nunca é escrito
  em arquivo de configuração.
- O arquivo de configuração não pode ter permissão de escrita para grupo/outros.
