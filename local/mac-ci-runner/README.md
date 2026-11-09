# Аварийный CI-раннер на Mac

Один эфемерный GitHub Actions раннер в Docker Desktop на Mac (arm64), когда ARC-пулу на 5950x не
хватает памяти CI-ноды. Первый пользователь — `Bafff/leiloes-radar`.

Лейбл раннера — имя ARC-пула, `leiloes-5950x`. Job с `runs-on: leiloes-5950x` уходит на любой
свободный из двух; ни пул, ни workflow менять не нужно. Проверено 2026-11-09: задача из очереди
досталась Mac, пока оба пода пула были заняты. Это наблюдаемое поведение GitHub, а не
документированное: если оно изменится, Mac просто будет простаивать, CI не сломается.

```sh
local/mac-ci-runner/mac-ci-runner.sh up        # Docker Desktop (если не запущен), dind, цикл раннера
local/mac-ci-runner/mac-ci-runner.sh status
local/mac-ci-runner/mac-ci-runner.sh down      # ждёт текущую job; --now убивает её, --prune чистит кэш образов
```

## Как устроено

- Повторяет под ARC из `helm/runners/values-leiloes.yaml`: контейнер `leiloes-ci-dind` (privileged
  dockerd с теми же аргументами) и `leiloes-ci-runner` в его сетевом namespace. Поэтому порт
  `services:` (`127.0.0.1:5432`) доступен job так же, как в поде. Образы и digest-пины те же:
  оба — multi-arch с arm64. Лимиты памяти 2g + 2g, как requests = limits в поде.
- Раннер регистрируется just-in-time через `gh api …/generate-jitconfig` (токен `gh` со scope
  `repo`), берёт одну job и удаляется. Цикл заводит следующий. Хранимых секретов нет.
  JIT-конфиг передаётся через `ACTIONS_RUNNER_INPUT_JITCONFIG` в env-файле, не в argv.
- `_work` — именованный том, перед каждой job очищается и отдаётся uid 1001 (в поде это свежий
  emptyDir). `/var/lib/docker` dind'а живёт между job, так что образы postgres и python не
  скачиваются заново; `down --prune` его удаляет.
- Цикл работает из копии скрипта в `~/Library/Application Support/leiloes-ci-runner/`, лог —
  `~/Library/Logs/leiloes-ci-runner.log`. Пока цикл жив, `caffeinate -i` не даёт Mac уснуть от
  простоя (закрытая крышка на батарее всё равно усыпит).
- Автозапуска нет: только по необходимости.

## Ограничения

- Выключение Mac посреди job: через ~10 минут она падает с «runner lost communication». Это не про
  код. `down` ждёт job.
- Память: CI добавляет до ~4 GB к VM Docker Desktop. При забитом свопе сессии на Mac тормозят;
  если job на Mac идёт больше чем вдвое дольше, чем на 5950x, — `down`.
- Код PR выполняется в privileged-контейнере. Побег из него ведёт в VM Docker Desktop, а она
  видит общий доступ к `/Users`. Принято как есть: репозиторий приватный, пушит только владелец.
- Когда Mac забирает job, listener ARC мог уже заказать под под неё; такой под простаивает, пока
  его не уберёт масштабирование, и держит память ноды.
- Другой репозиторий: `REPO=… LABEL=… PREFIX=… mac-ci-runner.sh up`. Job должна быть Linux-only
  и годиться для arm64.
