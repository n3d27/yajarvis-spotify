# Soloist `ctl` verbs

Снято с колонки командой `soloist ctl --help` (только чтение). Вызов: `LD_LIBRARY_PATH=<dir>/lib64 <dir>/bin/soloist ctl -w 127.0.0.1:9090 <verb>`.

| Глагол | Назначение |
|---|---|
| `status` | состояние демона |
| `now [--json]` | текущее воспроизведение |
| `queue [n]` | ближайшие треки (по умолчанию 10) |
| `trace` | поток событий WS |
| `play [uri]` | продолжить или включить URI |
| `pause` | пауза |
| `next` / `prev` | следующий / предыдущий трек |
| `seek <ms>` | перемотка в миллисекундах |
| `volume <0-100>` | громкость |
| `shuffle <on\|off>` | перемешивание |
| `repeat <off\|context\|track>` | повтор |
| `add-to-queue <uri>` | в очередь |
| `activate` / `deactivate` | стать / перестать быть активным устройством Spotify Connect |

Опции: `-D/--data-dir <path>`, `-w/--ws <addr>:<port>`, `-h/--help`.

## Как плагин их использует

- Основной путь: `sh <dir>/scripts/control.sh <play [uri]|pause|next|prev|shuffle on|off>` с cwd `/`. Скрипт сам делает `activate` + 0.3 с перед `play` и двойной `prev` после 3 с проигрывания.
- Запасной путь без `control.sh`: те же шаги самим плагином через `ctl`: `activate`, пауза 0.3 с, `play [uri]`; для `prev` при позиции >= 3 с два `prev` с паузой 0.3 с.
- `seek <ms>` всегда идёт напрямую через `ctl` (`control.sh` его не знает).
- Состояние: `ctl now --json` (таймаут 4 с); остальные вызовы 8 с, по таймауту убивается вся группа процессов.
