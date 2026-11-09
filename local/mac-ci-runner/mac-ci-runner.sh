#!/usr/bin/env bash
# Emergency CI capacity on the Mac: one ephemeral GitHub Actions runner in Docker Desktop for a
# repository whose ARC pool is short of memory (first user: Bafff/leiloes-radar, label leiloes-ci).
# The layout copies the ARC dind pod (helm/runners/values-leiloes.yaml): a privileged dockerd
# container, and a runner container in its network namespace, so a job's `services:` port
# (127.0.0.1:5432) is reachable exactly as in the pod. Memory limits match the pod's.
#
#   mac-ci-runner.sh up             start Docker Desktop if needed, dind, and the runner loop
#   mac-ci-runner.sh down [--now] [--prune]
#                                   stop after the current job (--now: kill it, the job goes red;
#                                   --prune: also drop the image cache volume)
#   mac-ci-runner.sh status
#
# Each runner is registered just in time through `gh` (token with `repo` scope) and takes one job.
# Nothing starts at login. Details: README.md next to this script.
set -euo pipefail

REPO=${REPO:-Bafff/leiloes-radar}
LABEL=${LABEL:-leiloes-ci}
PREFIX=${PREFIX:-leiloes-ci}
# The pod's images (values-leiloes.yaml); both digests are multi-arch indexes with arm64.
RUNNER_IMAGE=ghcr.io/actions/actions-runner:2.338.0@sha256:4ffadc0002b2581327e06101fc8c06cd189232baf79fe561fac9caeb76f5e807
DIND_IMAGE=docker:29.9.0-dind@sha256:0b6d18a4a222e71c88be6034926b4f6cde2fc33c2218643d03ccfddc65995470
STATE=${STATE:-"$HOME/Library/Application Support/$PREFIX-runner"}
LOG=${LOG:-"$HOME/Library/Logs/$PREFIX-runner.log"}

DIND=$PREFIX-dind
RUNNER=$PREFIX-runner
VOL_DOCKER=$PREFIX-docker
VOL_SOCK=$PREFIX-sock
VOL_WORK=$PREFIX-work

log() { printf '%s %s\n' "$(date '+%F %T')" "$*"; }
die() { log "error: $*" >&2; exit 1; }

loop_pid() {
  local pid
  pid=$(cat "$STATE/loop.pid" 2>/dev/null) || return 1
  kill -0 "$pid" 2>/dev/null && echo "$pid"
}

ensure_docker() {
  docker info >/dev/null 2>&1 && return
  log "starting Docker Desktop"
  open -g -a Docker
  for _ in $(seq 90); do
    docker info >/dev/null 2>&1 && return
    sleep 2
  done
  die "Docker Desktop did not come up in 180 s"
}

start_dind() {
  docker rm -f "$DIND" >/dev/null 2>&1 || true
  # A fresh socket volume: a docker.pid left in /var/run by a killed dockerd stops the next one.
  docker volume rm -f "$VOL_SOCK" >/dev/null 2>&1 || true
  docker volume create "$VOL_SOCK" >/dev/null
  docker volume create "$VOL_DOCKER" >/dev/null
  docker volume create "$VOL_WORK" >/dev/null
  # dockerd arguments as in the pod; --group 123 is the runner image's docker group.
  docker run -d --name "$DIND" --privileged --memory 2g \
    -v "$VOL_DOCKER:/var/lib/docker" -v "$VOL_SOCK:/var/run" -v "$VOL_WORK:/home/runner/_work" \
    "$DIND_IMAGE" dockerd --host=unix:///var/run/docker.sock --group=123 \
    --mtu=1280 --default-network-opt=bridge=com.docker.network.driver.mtu=1280 >/dev/null
  for _ in $(seq 60); do
    docker exec "$DIND" docker info >/dev/null 2>&1 && return
    sleep 1
  done
  die "dockerd in $DIND not ready after 60 s"
}

# The pod's emptyDir is fresh and writable for the runner user (uid 1001); a named volume is
# neither, so it is wiped and handed over before every job.
reset_work() {
  docker run --rm -u 0 --entrypoint sh -v "$VOL_WORK:/w" "$RUNNER_IMAGE" \
    -c 'find /w -mindepth 1 -delete && chown 1001:123 /w'
}

run_one_job() {
  local name resp id envfile started
  name="mac-$(date +%Y%m%d-%H%M%S)"
  resp=$(gh api -X POST "repos/$REPO/actions/runners/generate-jitconfig" \
    -f name="$name" -F runner_group_id=1 -f "labels[]=$LABEL" -f work_folder=_work) || return 1
  id=$(jq -r '.runner.id // empty' <<<"$resp")
  [ -n "$id" ] || return 1
  echo "$id" >"$STATE/runner.id"
  # The JIT config reaches the runner through the environment, not argv (ARC does the same).
  envfile=$(mktemp "$STATE/jit.XXXXXX")
  printf 'ACTIONS_RUNNER_INPUT_JITCONFIG=%s\n' "$(jq -r .encoded_jit_config <<<"$resp")" >"$envfile"
  log "runner $name (id $id) waiting for a job"
  started=$SECONDS
  docker run --rm --name "$RUNNER" --network "container:$DIND" --memory 2g \
    --env-file "$envfile" -e DOCKER_HOST=unix:///var/run/docker.sock \
    -v "$VOL_SOCK:/var/run" -v "$VOL_WORK:/home/runner/_work" \
    "$RUNNER_IMAGE" /home/runner/run.sh || log "runner $name exited with $?"
  rm -f "$envfile" "$STATE/runner.id"
  # A runner that dies at once (bad image, no network) must not spin the loop.
  [ $((SECONDS - started)) -ge 15 ] || return 1
}

loop() {
  trap 'docker rm -f "$RUNNER" >/dev/null 2>&1; exit 0' TERM INT
  # No idle sleep while the loop lives; caffeinate exits with it.
  caffeinate -i -w $$ &
  while [ ! -e "$STATE/stop" ]; do
    if [ "$(docker inspect -f '{{.State.Running}}' "$DIND" 2>/dev/null)" != true ]; then
      log "dind is not running, restarting it"
      start_dind || { sleep 30; continue; }
    fi
    if ! reset_work || ! run_one_job; then
      log "iteration failed, retrying in 30 s"
      sleep 30
    fi
  done
  log "stop requested, loop exits"
}

runner_busy() {
  local id
  id=$(cat "$STATE/runner.id" 2>/dev/null) || { echo false; return; }
  gh api "repos/$REPO/actions/runners/$id" --jq .busy 2>/dev/null || echo false
}

deregister() {
  local id
  id=$(cat "$STATE/runner.id" 2>/dev/null) || return 0
  gh api -X DELETE "repos/$REPO/actions/runners/$id" >/dev/null 2>&1 || true
  rm -f "$STATE/runner.id"
}

cmd_up() {
  mkdir -p "$STATE" "$(dirname "$LOG")"
  if pid=$(loop_pid); then log "already up (loop pid $pid)"; return; fi
  for tool in gh jq docker; do command -v "$tool" >/dev/null || die "needs $tool"; done
  gh auth status >/dev/null 2>&1 || die "gh is not logged in"
  ensure_docker
  rm -f "$STATE/stop"
  start_dind
  # The loop runs from a copy, so editing or removing the checkout cannot break a running loop.
  cp "$0" "$STATE/mac-ci-runner.sh"
  export REPO LABEL PREFIX STATE LOG
  nohup bash "$STATE/mac-ci-runner.sh" _loop >>"$LOG" 2>&1 &
  echo $! >"$STATE/loop.pid"
  log "up: label $LABEL on $REPO, log $LOG"
}

cmd_down() {
  local now=false prune=false
  for a in "$@"; do
    case $a in
      --now) now=true ;;
      --prune) prune=true ;;
      *) die "unknown flag $a" ;;
    esac
  done
  mkdir -p "$STATE"
  touch "$STATE/stop"
  if docker ps -q -f "name=^$RUNNER\$" 2>/dev/null | grep -q .; then
    if [ "$(runner_busy)" = true ] && ! $now; then
      log "runner is busy; waiting for its job to finish (down --now kills it)"
      while docker ps -q -f "name=^$RUNNER\$" | grep -q .; do sleep 5; done
    else
      docker rm -f "$RUNNER" >/dev/null 2>&1 || true
    fi
  fi
  deregister
  if pid=$(loop_pid); then kill "$pid" 2>/dev/null || true; fi
  rm -f "$STATE/loop.pid"
  docker rm -f "$DIND" >/dev/null 2>&1 || true
  if $prune; then docker volume rm -f "$VOL_DOCKER" "$VOL_SOCK" "$VOL_WORK" >/dev/null 2>&1 || true; fi
  log "down. Docker Desktop is still running; quit it with: osascript -e 'quit app \"Docker\"'"
}

cmd_status() {
  if pid=$(loop_pid); then echo "loop: up (pid $pid)"; else echo "loop: down"; fi
  local st
  st=$(docker inspect -f '{{.State.Status}}' "$DIND" 2>/dev/null) || st=absent
  echo "dind: $st"
  if id=$(cat "$STATE/runner.id" 2>/dev/null); then
    echo "runner: id $id, busy=$(runner_busy)"
  else
    echo "runner: none registered"
  fi
  echo "log: $LOG"
}

case ${1:-} in
  up) cmd_up ;;
  down) shift; cmd_down "$@" ;;
  status) cmd_status ;;
  _loop) loop ;;
  *) sed -n '2,15p' "$0"; exit 2 ;;
esac
