# opencc

Wrapper bash per avviare **Claude Code** su backend alternativi. Unifica i due
script precedenti (`cc-go` e `opencc`) in un unico tool con tre backend:

| Backend     | Cosa usa | Autenticazione | Proxy |
|-------------|----------|----------------|-------|
| `openai`    | modelli OpenAI (GPT-5.x) | abbonamento ChatGPT (OAuth Codex) o `OPENAI_API_KEY` | sì (`opencc-proxy.mjs`, traduzione Anthropic→Responses) |
| `go`        | gateway [opencode-go](https://opencode.ai/zen/go) di OpenCode | header `x-api-key` | sì (pass-through Anthropic) |
| `anthropic` | Claude Code standard | invariata (comportamento nativo) | no |

Nei backend `openai` e `go`: menu numerato dei modelli con la dimensione del
contesto, scelta del **livello di ragionamento**, memoria dell'ultima scelta
(per backend) e configurazione automatica delle variabili d'ambiente di Claude
Code. Il backend `anthropic` è un pass-through puro: lancia `claude` senza
toccare endpoint, autenticazione, modello, effort o settings.

## Installazione

Copiare `opencc` **e** `opencc-proxy.mjs` nella stessa directory del `PATH`
(es. `~/.local/bin/`) e rendere eseguibile `opencc`:

```bash
cp opencc opencc-proxy.mjs ~/.local/bin/
chmod +x ~/.local/bin/opencc
```

I due file devono restare affiancati: `opencc` cerca `opencc-proxy.mjs` nella
propria directory (serve ai backend `openai` e `go`).

## Prerequisiti

- `claude` installato (Claude Code)
- `curl` e `python3` (elenco modelli; niente `jq`) — non servono per il backend
  `anthropic`, che ha zero dipendenze oltre a `claude`
- backend `openai`/`go`: `node` ≥ 18 per il proxy
- backend `openai`:
  - **subscription (predefinita):** il [CLI Codex](https://github.com/openai/codex)
    per il login. Il token OAuth sta in `~/.codex/auth.json` (scritto da
    `opencc login`); la lista modelli è letta da `~/.codex/models_cache.json`.
  - **apikey:** una chiave in `OPENAI_API_KEY`.
- backend `go`: una API key in `OPENCODE_API_KEY` oppure il file
  `~/.local/share/opencode/auth.json` (login con `opencode`).

## Utilizzo

```bash
opencc login                  # genera/rinnova ~/.codex/auth.json (device flow)
opencc [argomenti per claude] # menu backend + modelli + ragionamento, poi avvia
```

All'avvio `opencc` chiede:

1. il **backend**:
   - `1` `openai` — modelli OpenAI (proxy locale);
   - `2` `go` — gateway OpenCode Go;
   - `0` `anthropic` — Claude Code standard (pass-through, nessuna modifica).

   Il default è l'ultimo usato; il menu si salta impostando
   `OPENCC_BACKEND=openai|go|anthropic`.
2. il **modello**, con contesto e segno dell'ultimo usato;
3. il **livello di ragionamento** valido per quel modello.

Premendo invio (o `d`) si usano i default. Sul backend `go` i modelli con
ragionamento sempre attivo (es. `minimax-m3`) non mostrano il punto 3.

## Cambiare modello e ragionamento durante la sessione

Le scelte fatte al lancio sono solo i **default della sessione**: `/model` e
`/effort` restano attivi e valgono per le richieste successive, senza riavviare.
`opencc` genera per il backend corrente un file `model-picker.json` e lo carica
tramite `--settings`: `/model` mostra così i modelli OpenAI o OpenCode Go invece
degli alias Anthropic standard. Ogni voce include, nella descrizione, gli
**effort realmente supportati** dal modello e il suo default (es.
`OpenAI via opencc · effort: low, medium, high, xhigh, max (default: medium)`);
i modelli senza ragionamento configurabile sono marcati `effort: non
configurabile`.

La discovery automatica di Claude Code non viene usata: anche se il backend
espone correttamente `GET /v1/models`, Claude Code scarta per design tutti gli ID
che non contengono `claude` o `anthropic` (quindi `gpt-*`, `minimax-*`, ecc.). Il
`modelPicker` nativo accetta invece ID arbitrari e li inoltra senza rinominarli.

### Limite: il picker `/effort` è globale

Claude Code non conosce le capability dei modelli custom, e **non** consente di
filtrare dinamicamente i livelli di `/effort` per il modello selezionato: le righe
di `modelPicker` accettano solo ID, etichetta e descrizione, e non esiste un modo
per dichiarare gli effort supportati o un default per ID arbitrari. Il picker
`/effort` resta quindi globale.

La correzione avviene nel proxy, che riceve ogni richiesta con il modello e
l'effort scelto e applica la **policy reale** del modello (da
`model-efforts.json`):

- livello non supportato → **ridotto** al massimo livello disponibile non
  superiore a quello richiesto (es. `xhigh` → `high` su un modello senza
  `xhigh`); se nessun livello è minore, al minimo disponibile;
- livello sconosciuto → default del modello;
- modello senza effort → l'effort viene **rimosso**;
- nessun effort scelto → **default** del modello.

Quando il proxy modifica l'effort, scrive una riga nel proprio log
(`~/.local/state/opencc/<backend>/proxy.log`), es.:
`[opencc] effort gpt-two: max -> high (clamped)`.

Altri limiti noti:

- **`ultra`** (solo GPT-5.6) non è un livello che `/effort` accetta: si può
  scegliere solo dal menu iniziale, dove viene codificato come `modello@ultra`.
- `CLAUDE_CODE_MAX_CONTEXT_TOKENS` viene impostato in base al modello scelto al
  lancio e non segue i cambi fatti con `/model`.

> **Nota:** l'effort viene passato con il flag `--effort` e **non** con la
> variabile `CLAUDE_CODE_EFFORT_LEVEL`: quella variabile inchioda il livello per
> l'intero processo e rende `/effort` inefficace (Claude Code continuerebbe a
> inviare il valore dell'env). Se è presente nell'ambiente, `opencc` la rimuove.

### Variabili d'ambiente

| Variabile | Effetto |
|-----------|---------|
| `OPENCC_BACKEND` | `openai` \| `go` \| `anthropic` — salta il menu backend |
| `OPENCC_MODE` | `subscription` \| `apikey` — forza l'autenticazione OpenAI |
| `OPENCC_PROXY_PORT` | porta del proxy locale (default `3199`, backend `openai`/`go`) |
| `OPENAI_API_KEY` | chiave OpenAI (modalità `apikey`) |
| `OPENCODE_API_KEY` | chiave del gateway OpenCode Go |

## Backend `openai`

Claude Code parla **solo** il protocollo Anthropic (`/v1/messages`), mentre i
backend OpenAI usano il protocollo Responses: `opencc-proxy.mjs` è un proxy
locale (solo `127.0.0.1`) che traduce le richieste e le inoltra a

- **subscription** → `https://chatgpt.com/backend-api/codex/responses`, col token
  OAuth di `~/.codex/auth.json` (usa il piano ChatGPT Plus/Pro/Team);
- **apikey** → `https://api.openai.com/v1/responses`, con `OPENAI_API_KEY`.

- **Login:** `opencc login` avvia il device flow del CLI Codex. Se manca
  l'autenticazione, `opencc` la propone all'avvio.
- **Refresh automatico:** i token durano ~24h; su 401 il proxy li rinnova via
  `refresh_token` e riscrive `~/.codex/auth.json`. Se il refresh fallisce, basta
  rifare `opencc login`.
- **Modelli:** da `~/.codex/models_cache.json` (subscription) o da `/v1/models`
  di OpenAI (apikey); fallback su una lista statica. Il proxy li espone anche a
  Claude Code, ma la discovery automatica non viene usata (vedi sopra).
- **Contesto:** `max_context_window × effective_context_window_percent` (es.
  ~828K per GPT-5.6), esportato in `CLAUDE_CODE_MAX_CONTEXT_TOKENS`.
- **Ragionamento:** Claude Code invia `output_config: { effort }` e il proxy lo
  traduce in `reasoning: { effort }`, normalizzandolo con la policy del modello
  (vedi sopra). `ultra` non è accettato da `--effort`/`/effort` (verrebbe
  ignorato in silenzio), quindi resta codificato nel modello come
  `modello@ultra`, formato che il proxy riconosce.
- **Collegamento tra turni (risparmio input):** il proxy non rimanda mai
  l'intera storia, come fa codex: se la nuova richiesta della stessa sessione è
  un'estensione della precedente, invia **solo il delta** con
  `previous_response_id`; il backend ricollega il contesto e fattura la parte
  ripetuta a tariffa cache. Se il collegamento fallisce (risposta scaduta,
  contesto cambiato), il proxy ritenta automaticamente con la richiesta completa.
  Il log registra ogni richiesta:
  `[opencc] delta sess-1|: 1 item inviati (baseline 2)` e
  `[opencc] usage gpt-5.6-sol: in=... cached=... out=... (delta|full)`.
  Prima di questo fix, ogni turno rimandava la storia completa via HTTP: se la
  cache automatica (TTL ~5 min) scadeva, l'intero contesto veniva rifatturato a
  prezzo pieno — da qui un consumo molto più alto di codex/opencode.
- **`/usage`:** i token (input/output) mostrati da `/usage` sono quelli reali del
  backend OpenAI: il proxy converte `input_tokens_details.cached_tokens` in
  `cache_read_input_tokens` e lo sottrae da `input_tokens`, come da convenzione
  Anthropic. Due limiti: le colonne **cache read/write** risultano 0 (la
  Responses API riporta l'usage solo a fine stream, mentre Claude Code legge la
  cache da `message_start`), e il **costo** è una stima di Claude Code per modelli
  sconosciuti, marcata `costs may be inaccurate due to usage of unknown models`
  — non è possibile iniettare la prezzatura del provider.

## Backend `go`

- **Modelli:** elenco da `/v1/models` del gateway; **contesto ed effort** dal
  catalogo upstream `https://models.opencode.ai/api.json` (provider
  `opencode-go`). Cache in `~/.local/state/opencc/go/models.tsv`, rinfrescata in
  background dopo 7 giorni; senza cache si usa il fallback `minimax-m3`.
- **Ragionamento:** i valori validi del modello vengono intersecati con quelli
  accettati da Claude Code (`low,medium,high,xhigh,max`).
- **`/usage`:** essendo un pass-through Anthropic, l'usage del gateway arriva a
  Claude Code senza conversioni: token e cache sono quelli riportati da
  opencode-go (se li include); il costo resta una stima per modelli sconosciuti.
- **Proxy pass-through:** `opencc` instrada il backend `go` attraverso lo stesso
  proxy locale, in modalità **pass-through Anthropic**: il proxy modifica solo
  `model` ed `output_config.effort` (applicando la policy) e inoltra al gateway
  il resto della richiesta e della risposta senza tradurle. Il proxy chiede
  `Accept-Encoding: identity` all'upstream e non inoltra `Content-Encoding`:
  evita così che il body già decompresso venga re-decompresso da Claude Code
  (BrotliDecompressionError). Il proxy autentica con `OPENCODE_API_KEY` (o il
  valore da `auth.json`) in `x-api-key`.

## Auto mode (classificatore)

L'auto mode di Claude Code usa il classificatore di sicurezza tramite l'alias
**haiku** con `max_tokens: 1`. I modelli con ragionamento consumano quel singolo
token nel `thinking` e non producono testo: il classificatore fallisce e l'auto
mode riporta *"auto mode cannot determine the safety"*. Per questo `opencc`
pinnà `ANTHROPIC_DEFAULT_HAIKU_MODEL` su un modello dedicato **senza
ragionamento**:

- backend `go` → il primo modello del catalogo senza livelli di effort
  (default `minimax-m3`);
- backend `openai` → il primo modello `*mini*` (default `gpt-5.4-mini`).

Il modello principale (opus/sonnet, subagent) non cambia. Il backend
`anthropic` usa il comportamento nativo.

## Chiusura automatica del proxy

Quando esci da Claude Code, `opencc` chiude il proxy **se non restano altre
sessioni attive** sullo stesso proxy (porta+modalità). Ogni invocazione registra
un file in `~/.local/state/opencc/<backend>/sessions/<pid>.sess`; all'uscita il
file viene rimosso e, se era l'ultimo, il proxy viene terminato. I file di
sessioni terminate in modo anomalo (PID non più vivo) vengono spazzati
all'avvio successivo. Il backend `anthropic` non usa il proxy: nessuna
registrazione.

## Backend `anthropic`

Selezionando `anthropic` `opencc` esegue `claude` senza alcuna modifica:
nessun proxy, nessuna variabile d'ambiente di gateway, nessun menu modelli e
nessun `--settings` generato. Il comportamento è quello nativo di Claude Code,
con l'eventuale configurazione del tuo ambiente. È l'unico backend senza
dipendenza da `node`/`curl`/`python3`.

## Stato locale

`~/.local/state/opencc/`:

```
last-backend         ultimo backend usato
openai/              last-model, last-effort, model-picker.json,
                     model-efforts.json, proxy.log, proxy.pid
go/                  last-model, last-effort, model-picker.json,
                     model-efforts.json, models.tsv, models.ids
```

Lo stato dei vecchi script viene migrato automaticamente al primo avvio
(`~/.local/state/opencc/*` → `openai/`, `~/.local/state/cc-go/*` → `go/`).

## Note

- **Piano gratis:** il backend subscription richiede un piano a pagamento
  ChatGPT (Plus/Pro/Team).
- La logica di traduzione di `opencc-proxy.mjs` è adattata dal proxy MIT-licensed
  di [codex-for-claude-code](https://github.com/Yusang-park/codex-for-claude-code).
- Test del proxy: `node --test opencc-proxy.test.mjs`.
