# Управление секретами для GitHub Runners

Раннерам нужен доступ к вашему GitHub репозиторию или организации для регистрации и прослушивания очереди задач.

## Способ 1: Personal Access Token (PAT) — Самый быстрый

1. Перейдите в GitHub: **Settings** -> **Developer Settings** -> **Personal access tokens** -> **Tokens (classic)**.
2. Нажмите **Generate new token (classic)**.
3. Задайте имя (например `k8s-arc-runner`) и выберите права:
   - Для отдельного репозитория: `repo` (полный доступ к репозиторию).
   - Для всей организации: `admin:org` (полный доступ к управлению организацией).
4. Создайте секрет в Kubernetes одной командой:
   ```bash
   kubectl create namespace arc-runners
   kubectl create secret generic arc-runner-secret \
     --namespace arc-runners \
     --from-literal=github_token="ghp_ВАШ_ТОКЕН"
   ```

## Способ 2: GitHub App (Рекомендуется для безопасности)
Токены GitHub App автоматически ротируются каждые 60 минут и не привязаны к личной учетной записи.
Подробная инструкция в официальной документации:
https://docs.github.com/en/actions/hosting-your-own-runners/managing-self-hosted-runners-with-actions-runner-controller/authenticating-to-the-github-api
