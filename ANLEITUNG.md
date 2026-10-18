# ANLEITUNG – LAN Verse

Kurzanleitung für Spieler, Server-Verantwortliche und Modus-Bauer.
Funktionen, Befehle und die wichtigsten Abläufe.

---

## 1. Übersicht

```
LANVerseGame      Server: Simulation, Lua-Modi, WebSocket, REST-API, Admin-Seite
LANVerseClient    Spiel (Rust + raylib)
LANVerseBuilder   Modus-Editor und Upload (`publish`)
```

Ein Server-Prozess genügt:

| Port | Zweck |
|---|---|
| `0.0.0.0:8080` | Spiel (WebSocket) + REST-API unter `/api/v1` |
| `127.0.0.1:8081` | Admin-Seite + interner Ergebnis-Endpunkt (nur lokal) |

Postgres speichert Konten, Modi, Matches und Highscores; Redis speichert
laufende Welten (Neustart mitten in der Runde) und den Modus-Cache. Beide sind
optional: ohne Datenbank läuft ein reines Deathmatch mit Gästen.

Voraussetzung: aktuelle Rust-Toolchain (`cargo`). Alles wird aus dem Quellcode
gebaut; es gibt keine weiteren Abhängigkeiten außer Postgres/Redis.

---

## 2. Schnellstart (5 Minuten)

```sh
# Terminal 1 – Server
cd LANVerseGame
cargo run --release -- serve
#   Spiel:  http://127.0.0.1:8080
#   Admin:  http://127.0.0.1:8081

# Terminal 2 – Spieler
cd LANVerseClient
cargo run -- --player-name Alice
# weitere Spieler: gleicher Befehl mit anderem Namen
```

Für die erste Ausführung einmal übersetzen (danach startet es schneller):

```sh
(cd LANVerseGame && cargo build --release)
(cd LANVerseClient && cargo build --release)
(cd LANVerseBuilder && cargo build --release)
```

Ohne weitere Konfiguration landet man im gemeinsamen Deathmatch. Der Client
verbindet sich automatisch neu, wenn der Server (noch) nicht läuft.

---

## 3. Server (`LANVerseGame`)

### 3.1 Befehle

```sh
cd LANVerseGame

cargo run --release -- serve         # WebSocket + REST + Admin (Standard)
cargo run --release -- daemon        # nur Simulation, Frames über Redis (opt-in)
cargo run --release -- migrate                  # Daten aus dem alten MySQL importieren
cargo run --release -- migrate --check          # Import nur anzeigen (Trockenlauf)
cargo run --release -- token --email=you@example.test --name=Du
cargo run --release -- token --revoke --email=you@example.test
cargo run --release -- mode --list
cargo run --release -- mode coin_rush --owner=you@example.test
cargo run --release -- --help
```

`token`, `mode` und `migrate` brauchen `LANBASH_DATABASE_URL` (in derselben
Shell gesetzt oder vorangestellt, z. B.
`LANBASH_DATABASE_URL=… cargo run --release -- token --email=…`).

Mit Datenbank und vollem Funktionsumfang:

```sh
cd LANVerseGame
export LANBASH_DATABASE_URL="postgres://lanverse:secret@127.0.0.1/lanverse"
export LANBASH_REDIS_URL="redis://127.0.0.1:6379/2"
export LANBASH_RESULT_SECRET="$(openssl rand -hex 32)"   # signiert Rundenergebnisse
export LANBASH_ADMIN_KEY="$(openssl rand -hex 16)"       # Admin-Seite
cargo run --release -- serve
```

Das Schema wird beim Start automatisch angelegt. Die Datenbank `lanverse` muss
vorher existieren. `migrate` importiert Daten aus einer MySQL-Quelle
(`LANBASH_MYSQL_URL`) und legt dabei das Schema an, falls nötig; ohne Quelle
passiert nichts (siehe Abschnitt 3.6).

### 3.2 Die wichtigsten Einstellungen (Umgebungsvariablen)

| Variable | Standard | Bedeutung |
|---|---|---|
| `LANBASH_BIND` | `0.0.0.0:8080` | Spiel-Port |
| `LANBASH_ADMIN_BIND` | `127.0.0.1:8081` | Admin-Port (nur lokal!) |
| `LANBASH_DATABASE_URL` | – | Postgres; aktiviert Konten, Modi, Ergebnisse |
| `LANBASH_REDIS_URL` | `redis://127.0.0.1:6379/2` | Welten-Cache, Modus-Cache |
| `LANBASH_RESULT_SECRET` | – | HMAC-Schlüssel für Rundenergebnisse |
| `LANBASH_ADMIN_KEY` | – | Login für die Admin-Seite |
| `LANBASH_TICK_RATE` | `60` | Simulation pro Sekunde |
| `LANBASH_SNAPSHOT_RATE` | `20` | Snapshots pro Sekunde |
| `LANBASH_MAX_PLAYERS` | `512` | Spieler pro Match |
| `LANBASH_DEFAULT_MATCH` | `lan` | Präfix des gemeinsamen Matches (`lan-JJJJMMTT`) |
| `LANBASH_ROUND_DURATION` | `180000` | Rundenlänge Deathmatch (ms) |
| `LANBASH_ROUND_COUNTDOWN` | `3000` | Countdown (ms) |
| `LANBASH_MIN_PLAYERS` | `1` | Spieler bis die Runde startet |
| `LANBASH_WORLD_WIDTH` / `_HEIGHT` | `3200` / `1800` | Deathmatch-Feld |
| `LANBASH_MODE_ALLOW_PUBLISH` | `true` | `false` = jeder Upload bleibt Entwurf |
| `LANBASH_MYSQL_URL` | – | Quelle für `migrate` (`mysql://user:pass@host/db`) |
| `LANBASH_PUBLIC_URL` | `http://127.0.0.1:8080` | wird bei `token` angezeigt |
| `RUST_LOG` | `info` | z. B. `debug` für Frames, `trace` für Ticks |

Die vollständige Liste steht in `LANVerseGame/README.md`.

### 3.3 Admin-Seite

Browser: `http://127.0.0.1:8081/admin` (nur auf dem Server selbst erreichbar;
von einem anderen Rechner per SSH-Tunnel:
`ssh -L 8081:127.0.0.1:8081 benutzer@server`).

| Funktion | Beschreibung |
|---|---|
| Login | Admin-Schlüssel (`LANBASH_ADMIN_KEY`), Session-Cookie |
| Übersicht | Welten, Spieler, Tick-Rate, Protokollversion, Dienst-Status |
| Modusliste | id, Name, Status (live/Entwurf), Besitzer, Checksumme |
| Upload | `mode.json` + `script.lua` einfügen, Ergebnis der Prüfung anzeigen |
| Publish/Unpublish | Modus live schalten bzw. aus der Liste nehmen |
| Besitz übertragen | Modus einem anderen Konto zuordnen |
| Löschen | nur Entwürfe (live zuerst unpublizieren) |
| Tokens | Builder-Token für eine E-Mail erzeugen/widerrufen |

Ohne gesetzten `LANBASH_ADMIN_KEY` ist die Seite gesperrt.

### 3.4 REST-API (`/api/v1`)

Öffentlich:

| Methode | Pfad | Zweck |
|---|---|---|
| GET | `/health` | Status (Datenbank, Redis, Welten, Spieler) |
| GET | `/modes` | veröffentlichte Modi (`?include_drafts=1`: alle) |
| GET | `/modes/{id}` | komplettes Modus-Paket |
| POST | `/auth/register` | `{name, email, password, password_confirmation}` → Token |
| POST | `/auth/login` | `{email, password}` → Token |

Mit Token (`Authorization: Bearer <token>`):

| Methode | Pfad | Zweck |
|---|---|---|
| GET | `/auth/me` · POST `/auth/logout` | eigenes Konto / abmelden |
| GET/PATCH | `/profile` | Profil lesen / Anzeigename, Avatar ändern |
| GET | `/profile/cosmetics` · PUT `/profile/cosmetics/{slug}` | Kosmetik / ausrüsten |
| GET/POST | `/friends` · DELETE `/friends/{id}` | Freunde |
| GET/POST | `/lobbies` | Lobbys (Wartezimmer vor dem Match) |
| GET | `/matches/{id}` · `/matches/{id}/results` | Match + Runden |
| GET | `/highscores?game_mode=&limit=` | Bestenliste |
| POST | `/modes` | Upload `{manifest, script, publish}` (Builder) |
| POST | `/modes/{id}/publish` · `/unpublish` · DELETE | Modus-Verwaltung (nur Besitzer) |

Beispiel:

```sh
# Konto anlegen
curl -s -X POST http://127.0.0.1:8080/api/v1/auth/register \
  -H 'Content-Type: application/json' \
  -d '{"name":"Alice","email":"alice@example.test","password":"supersecret123","password_confirmation":"supersecret123"}'

# Profil abfragen
curl -s http://127.0.0.1:8080/api/v1/auth/me -H "Authorization: Bearer <token>"

# Bestenliste
curl -s "http://127.0.0.1:8080/api/v1/highscores?game_mode=deathmatch&limit=10" \
  -H "Authorization: Bearer <token>"
```

Validierungsfehler kommen als `422` mit `{"message": …, "errors": {"feld": ["…"]}}`.

### 3.5 Runden und Ergebnisse

Wenn eine Runde endet, wird das Ergebnis (Gewinner, Team-Punkte, Spieler)
kanonisch als JSON gebaut, mit HMAC-SHA256 signiert und nur bei gültiger
Signatur gespeichert. Doppelt gesendete Ergebnisse werden ignoriert
(idempotent pro Match und Runde). Highscores entstehen aus diesen Ergebnissen.

### 3.6 Daten aus dem alten Server übernehmen

```sh
export LANBASH_DATABASE_URL="postgres://lanverse:secret@127.0.0.1/lanverse"
export LANBASH_MYSQL_URL="mysql://root@127.0.0.1/lanbash"

cargo run --release -- migrate --check   # nur Bericht: Zeilen alt/neu je Tabelle
cargo run --release -- migrate           # importieren (wiederholbar)
```

`migrate` ohne `LANBASH_MYSQL_URL` tut nichts und erklärt das auch — das Schema
wird von `serve` angelegt.

Konten, Profile, Tokens, Kosmetik, Freunde, Lobbys, Matches, Runden,
Highscores und Modi werden übernommen. Tokens bleiben gültig; Passwörter mit
altem Hash-Verfahren werden beim nächsten Login automatisch aktualisiert.

### 3.7 Betrieb

- **Neustart:** laufende Welten werden beim Beenden nach Redis gespeichert und
  beim nächsten Join wieder aufgenommen.
- **Sicherung:** Postgres ist der einzige dauerhafte Speicher (`pg_dump`).
- **Logs:** Ausgabe auf stdout; Ticks über 5 ms werden mit Match und
  Spielerzahl gemeldet (`RUST_LOG=debug` für mehr Details).
- **Firewall:** nur Port 8080 nach außen öffnen; 8081 bleibt lokal.

---

## 4. Client (`LANVerseClient`)

### 4.1 Start

```sh
cd LANVerseClient
cargo run -- --player-name Alice                     # Standard: ws://127.0.0.1:8080/
cargo run -- --server 192.168.1.20:8080 --player-name Alice
cargo run -- --mode coin_rush --player-name Alice    # Lua-Modus spielen
cargo run -- --token "12|AbC…" --player-name Alice   # mit eigenem Konto
cargo run -- --guest                                 # Gast mit Zufallsnamen
cargo run -- --headless --max-ticks 600              # ohne Fenster (Test/CI)
cargo run -- --mock-server                           # Demo ohne Server
cargo run -- --help
```

| Option | Bedeutung |
|---|---|
| `-s, --server <URL>` | Endpunkt: `host:port`, `http://host:port` oder `ws://…` |
| `-n, --player-name <NAME>` | Anzeigename im Handshake |
| `--token <TOKEN>` | Konto-Token (aus Login oder `token`-Befehl) |
| `--guest` | ohne Token, zufälliger Gastname |
| `--mode <ID>` | Modus-Match beitreten (`deathmatch` = normales Spiel) |
| `--headless` | kein Fenster, nur Netzwerk/Vorhersage |
| `--mock-server` | eingebauter Demo-Server |
| `--max-ticks <N>` | headless nach N Ticks beenden |
| `--max-frames <N>` | GUI nach N Bildern beenden |

Umgebungsvariablen: `LAN_BASH_SERVER`, `LAN_BASH_PLAYER_NAME`,
`LAN_BASH_TOKEN`, `LAN_BASH_MODE`, `RUST_LOG`.

### 4.2 Steuerung

| Taste | Aktion |
|---|---|
| WASD / Pfeiltasten | bewegen |
| Leertaste | Sprung (`jump`) |
| Linke Shift-Taste | Dash (`dash`) |
| E | Angriff / Interagieren (`interact`) |
| F | Item benutzen (`use_item`) |
| F1 | Debug-Overlay ein/aus |

### 4.3 Verhalten

- Der Client entscheidet nichts: Position, Treffer, Punkte und Rundenende
  kommen ausschließlich vom Server. Lokal wird nur vorhergesagt und
  interpoliert (weiche Bewegung).
- Verbindungsabbrüche werden automatisch neu aufgebaut (max. alle 2 s).
- Ein Modus-Paket wird einmal pro Sitzung heruntergeladen und unter
  `./modes/<id>/` abgelegt; eine vorhandene lokale Version wird nie
  überschrieben.
- Oben rechts ein kleines HUD: Verbindung, Tick, eigenes Konto, Modusname,
  Modus-Konsole (`mode_log`/`mode_error`), Mini-Map.

---

## 5. Builder (`LANVerseBuilder`)

Der Builder ist ein kleiner Editor für Lua-Modi mit Live-Vorschau.
Details: `LANVerseBuilder/README.md`.

### 5.1 Starten

```sh
cd LANVerseBuilder
cargo run                 # Editor
cargo run -- --preview    # direkt in die Vorschau
LAN_BASH_MODES_DIR=/pfad/zu/modes cargo run   # anderer Modus-Ordner
```

Beim ersten Start liegen im Ordner `modes/` die Beispiele `coin_rush` und
`king_of_the_hill`.

### 5.2 Aufbau

```
links            Modusliste (+ New Mode)
oben rechts      Formular: Name, Weltgröße, Tempo, Rundenlänge, Spieler, Bot-Anzahl
unten            Lua-Editor (script.lua)
oben             Schaltflächen: Validate, Run preview, Save (Ctrl+S)
```

### 5.3 Arbeitsablauf in 6 Schritten

1. **Modus anlegen** mit `+ New Mode` oder links einen bestehenden wählen.
   Die `id` ist der Ordnername unter `modes/` und bleibt fest; der Anzeigename
   ist frei änderbar.
2. **Metadaten** im Formular setzen (Weltgröße, Bewegungstempo, Max-HP,
   Rundenlänge, Countdown, Spielerzahl).
3. **Skript** im Editor schreiben. Hooks (alle optional):

   | Hook | Wann |
   |---|---|
   | `on_start()` | einmal beim Rundenstart |
   | `on_tick(dt)` | jeder Tick (60 Hz), `dt` in Sekunden |
   | `on_input(id, mx, my, action)` | bei akzeptierter Eingabe |
   | `on_finish(winner)` | einmal nach `lb.finish()` |

   Wichtigste Befehle (globales Table `lb`):
   `lb.log/warn`, `lb.world()`, `lb.players()`, `lb.player("p-local")`,
   `lb.set_position`, `lb.add_score`, `lb.set_score`, `lb.set_hp`,
   `lb.spawn_marker`, `lb.remove_marker`, `lb.clear_markers`, `lb.markers`,
   `lb.finish`, `lb.is_finished`, `lb.random`. Vollständige Tabelle im
   Builder-README und in `LANVerseGame/docs/mode-packages.md`.
4. **Validate** prüft Metadaten, Lua-Syntax und führt das Skript in einer
   Wegwerf-Welt aus. Fehler erscheinen in der Konsole.
5. **Run preview** startet die Vorschau (auch mit `F5`).
   WASD bewegt den Spieler, Leertaste = `interact`, Shift = `jump`,
   `R` startet neu, `Esc` verlässt das Eingabefeld.
6. **Ctrl+S** speichert `mode.json` und `script.lua` unter `modes/<id>/`.

### 5.4 Veröffentlichen (ein Befehl)

```sh
# 1. Server läuft (LANVerseGame, siehe Abschnitt 3)
# 2. Token besorgen – einmalig, danach wiederverwendbar:
cd LANVerseGame && cargo run -- token --email=builder@example.test --name=Builder
#    → 12|AbC…   (alternativ in der Admin-Seite erzeugen)

# 3. Hochladen
cd ../LANVerseBuilder
export LAN_BASH_TOKEN='12|AbC…'
cargo run -- publish modes/coin_rush
# → uploaded "modes/coin_rush" → coin_rush (live)

# Nur als Entwurf hochladen:
cargo run -- publish --draft modes/coin_rush

# Anderer Server:
cargo run -- publish --server http://192.168.1.20:8080 modes/coin_rush
```

Der Server prüft das Paket (Slug, Wertebereiche, Lua-Compile, Testlauf) und
meldet das Ergebnis. Ein ungültiger Upload wird gespeichert, aber **nicht**
veröffentlicht; ein bereits laufender Modus bleibt live.

### 5.5 Fehlermeldungen beim Publish

| Meldung | Lösung |
|---|---|
| `no LAN Verse server answers at …` | Server starten (`serve`) |
| `401 Unauthenticated` | Token fehlt/abgelaufen: `token`-Befehl oder Admin-Seite |
| `HTTP 422 … belongs to another account` | Slug gehört anderem Konto: anderes Token, umbenennen oder `mode <id> --owner=…` |
| `mode rejected: Lua load error …` | Syntaxfehler beheben (Zeile in der Meldung) |
| `mode rejected: Lua on_start error …` | Laufzeitfehler in `on_start` |

### 5.6 Danach spielen

```sh
cd ../LANVerseClient
cargo run -- --mode coin_rush --player-name Alice
```

Jeder veröffentlichte Modus ist ein eigenes Match; wer ohne `--mode` startet,
spielt Deathmatch. Ein neuer Upload gilt für **neue** Matches; ein laufendes
Match spielt weiter mit seiner Version.

---

## 6. Modi-Skripte: was der Server zusätzlich prüft

- Sandbox: kein `os`, `io`, `package`, `debug`, `load`, `require`.
- Budget: pro Hook maximal 2 000 000 Instruktionen und 250 ms; danach wird das
  Skript für das Match abgeschaltet (`mode_error`), die Runde läuft normal
  weiter.
- Werte werden begrenzt: Positionen im Feld, HP 0…max (0 = Tod), Punkte ≥ 0,
  höchstens 256 Marker.
- `"p-local"`/`"p-me"` stehen im Match für den ersten Spieler des Matches.
- Die Vorschau im Builder ist zum Ausprobieren – die Wahrheit liegt beim
  Server (`LANVerseGame/docs/mode-packages.md`).

---

## 7. Tests und Qualitätsprüfung

```sh
cd LANVerseGame    && cargo test            # Simulation (Referenz-Fixtures), Lua, Netzwerk, DB, Admin
cd ../LANVerseClient    && cargo test
cd ../LANVerseBuilder   && cargo test

# zusätzlich vor einem Commit:
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings

# optional: Leistung und Last
cd LANVerseGame
cargo test --release --test perf_test -- --ignored --nocapture
cargo test --release --test load_test -- --ignored --nocapture
```

Die Datenbank-Tests starten ein eingebettetes Postgres (beim ersten Lauf
heruntergeladen, danach aus dem Cache). Ohne Netz und ohne Cache überspringen
sie sich selbst.

---

## 8. Häufige Probleme

| Problem | Ursache / Lösung |
|---|---|
| Client verbindet nicht | Server läuft nicht oder anderer Port: `--server host:port`; Firewall Port 8080 |
| `401` in der API | Token fehlt/abgelaufen; neu erzeugen |
| `422` beim Registrieren | Passwort < 10 Zeichen oder E-Mail doppelt |
| `/admin` nicht erreichbar | nur auf `127.0.0.1:8081` (SSH-Tunnel für fern) |
| Modus erscheint nicht in der Liste | nicht veröffentlicht (Entwurf) oder Server ohne Datenbank |
| Modus startet nicht, „Mode not available“ | Modus nicht live, Tippfehler in `--mode` |
| Ergebnisse fehlen | `LANBASH_RESULT_SECRET` nicht gesetzt (kein Datenbankpfad) |
| Spielerzahl springt / Snapshots stocken | sehr viele Spieler in einem Match: auf mehrere Matches verteilen |
| Server neu gestartet, Runde weg | ohne Redis werden laufende Welten nicht gespeichert |
