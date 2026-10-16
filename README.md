# 🚀 GitHub Self-Hosted Runners on Kubernetes via ArgoCD & ARC

Декларативное GitOps-развертывание собственных GitHub Actions раннеров в Kubernetes с помощью **Actions Runner Controller (ARC)** и **ArgoCD**.

## ✨ Особенности
- **100% бесплатно:** Никаких плат за минуты и оркестрацию.
- **Autoscaling от 0:** Когда тестов нет — поды не запущены и не расходуют память хоста (`minRunners: 0`).
- **Изоляция и чистота (Ephemeral):** Каждый под создается строго под одну задачу и уничтожается после выполнения тестов.
- **GitOps через ArgoCD:** Паттерн *App-of-Apps* синхронизирует и контроллер, и раннеры автоматически из Git.
- **Гибкость по Docker:** Стандартный ультралегкий Linux-раннер по умолчанию + готовый конфиг для Docker-in-Docker (DinD), если позже понадобится собирать Docker-образы.

---

## 📁 Структура репозитория

```text
├── apps/                        # Манифесты ArgoCD Application
│   ├── root-app.yaml            # Корневое приложение (App-of-Apps)
│   ├── arc-controller.yaml      # ArgoCD App для контроллера ARC
│   └── arc-runners.yaml         # ArgoCD App для пула раннеров
├── helm/                        # Кастомные Helm values
│   ├── controller/
│   │   └── values.yaml          # Настройки контроллера (ресурсы, метрики)
│   └── runners/
│       ├── values.yaml          # Основной пул (легкий Linux-раннер)
│       └── values-dind.yaml     # Пул с поддержкой Docker-in-Docker
├── secrets/                     # Управление токенами и доступами
│   ├── github-secret.template.yaml
│   └── README.md
└── .github/workflows/           # Пример проверочного пайплайна
    └── example-test.yml
```

---

## Лимиты, образы и приоритет

- **CI-нода (с окна 2026-10-16, homelab-hosts spec `2026-10-16-ci-node-p3600-design.md`).** Пулы `idealista-5950x`, `idealista-5950x-light` и `5950x-k8s` идут только на ноду `k3d-5950x-ci-0` (`nodeSelector` + toleration `homelab.dev/pool=ci`), build — предпочтительно туда, deploy — на сервере. На CI-ноде прода нет, поэтому у pytest-пула requests = limits (8 GiB runner + 1.5 GiB dind, `maxRunners: 4`), CPU limit 19 = вся нода, а `PYTEST_XDIST_AUTO_NUM_WORKERS` = лимит CPU пода (для `pytest -n auto`). Цифры ниже про 16 GB WSL — история до окна.

- **Образы запинены** на тег и digest (`tag@sha256:...`): `actions-runner:2.338.0` и `docker:29.9.0-dind` (multi-arch index, есть linux/amd64). Обновлять вручную: `crane digest <image>:<tag>`.
- **Лимиты** (хост ограничен 16 GB RAM в WSL2): `idealista-5950x` (`maxRunners: 1` с 2026-10-14: два pytest-пода по ~3 GiB вызывали eviction; только джобы с Docker) runner 8 CPU / 5Gi (под `pytest -n 8`; `-n 4` было ~2.5 GiB по замеру 2026-10-12; 16 CPU / 8Gi под `-n 16` два пода рядом с продом не пережил k3d-контейнер), dind 2 CPU / 1536Mi (dockerd плюс один postgres-контейнер из `tests/test_migration_test_db_script.py`); `idealista-5950x-light` (`maxRunners: 3`) runner 2 CPU / 2Gi; `5950x-k8s` runner 2 CPU / 4Gi. С idealista-tracker#367 полный pytest идёт один раз на PR, так что два пода одновременно — исключение.
- **Limits не резервируют ресурсы.** Защита прод-нагрузки обеспечивается requests + PriorityClass (раннеры `ci-runner-low`, прод `prod-critical`) + kubelet eviction.
- **Диск:** у idealista-раннера ephemeral-storage request 1Gi / limit 10Gi, у dind 1Gi / 20Gi; emptyDir `work` (10Gi) и `/var/lib/docker` (20Gi) имеют `sizeLimit`, чтобы разросшаяся сборка вытеснялась, а не заполняла VHDX WSL.
- **Приоритет:** все runner-поды используют `priorityClassName: ci-runner-low`. Requires PriorityClass `ci-runner-low` from homelab-hosts PR #11; merge after it (под с несуществующим PriorityClass отклоняется на admission).

- **Токен ServiceAccount:** `idealista-5950x` запускает код из PR, поэтому в его pod template стоит `automountServiceAccountToken: false` (у PR-кода нет кластерных учётных данных).
- **Какой пул выбрать в `runs-on`** (Bafff/idealista-tracker): джобе нужен Docker (`services:`, `container:`, `docker build`/`compose`) или много CPU → `idealista-5950x`; нужен кластер → `idealista-5950x-deploy`; всё остальное (линтеры, проверки файлов, рендер манифестов) → `idealista-5950x-light`. Лёгкий пул без dind, requests 250m/512Mi, ephemeral-storage limit 5Gi, automount выключен (запускает код из PR). Ошибка заметна сразу: Docker-джоба на `-light` падает с `docker: not found`, а лёгкая джоба на `idealista-5950x` просто занимает один из двух dind-слотов, пока pytest ждёт.
- **Build-пул `idealista-5950x-build`** (`runs-on: idealista-5950x-build`, только job `build` из `deploy.yml` idealista-tracker: `docker build` и smoke-тест, без pytest): `minRunners: 1`, `maxRunners: 1`, чтобы деплой не ждал ни pytest на `idealista-5950x`, ни запуска пода. Runner 1 CPU / 1Gi (только git и docker CLI), dind 4 CPU / 1536Mi (BuildKit и smoke-тест: postgres, redis, приложение). Простаивающий под ниже своих requests, поэтому kubelet выселяет его последним. Automount выключен.
- **Deploy-пул `idealista-5950x-deploy`** (`runs-on: idealista-5950x-deploy`, репозиторий Bafff/idealista-tracker): обычный раннер без dind, requests 100m/256Mi, limits 1 CPU/1Gi, ephemeral-storage limit 2Gi, `maxRunners: 1`, automount включён, ServiceAccount `arc-runners/idealista-deployer` (создаётся здесь в `manifests/idealista-deployer/`, своих прав не имеет; Role привязывает homelab-hosts #15).

---

## 🛠️ Быстрый старт

### Шаг 1. Создайте токен GitHub и секрет в Kubernetes

Раннеру нужен токен для подключения к вашему репозиторию или организации:

1. В GitHub перейдите: **Settings** → **Developer Settings** → **Personal Access Tokens** → **Tokens (classic)**.
2. Создайте токен с областью действия `repo` (или `admin:org` для всей организации).
3. Примените секрет в кластер:
   ```bash
   kubectl create namespace arc-runners
   kubectl create secret generic arc-runner-secret \
     --namespace arc-runners \
     --from-literal=github_token="ghp_ВАШ_ТОКЕН"
   ```

### Шаг 2. Настройте URL вашего репозитория

1. В файле `helm/runners/values.yaml` замените URL на ваш:
   ```yaml
   githubConfigUrl: "https://github.com/ВАШ_АККАУНТ/ВАШ_РЕПОЗИТОРИЙ"
   ```
2. В файлах `apps/*.yaml` укажите URL этого Git-репозитория (откуда ArgoCD будет читать манифесты).

### Шаг 3. Запустите в ArgoCD

Примените корневой манифест (App-of-Apps):
```bash
kubectl apply -f bootstrap/root-app.yaml
```
ArgoCD автоматически:
- Поднимет неймспейс `arc-systems` и развернет `gha-runner-scale-set-controller`.
- Поднимет неймспейс `arc-runners` и подключит масштабный набор `arc-runner-set` к вашему репозиторию на GitHub.

*(Альтернатива без ArgoCD — обычный Helm CLI):*
<details>
<summary>Нажмите, чтобы развернуть команды чистого Helm</summary>

```bash
# 1. Контроллер
helm install arc \
  --namespace arc-systems \
  --create-namespace \
  -f helm/controller/values.yaml \
  oci://ghcr.io/actions/actions-runner-controller-charts/gha-runner-scale-set-controller

# 2. Раннеры
helm install arc-runner-set \
  --namespace arc-runners \
  --create-namespace \
  -f helm/runners/values.yaml \
  oci://ghcr.io/actions/actions-runner-controller-charts/gha-runner-scale-set
```
</details>

---

## 🧪 Проверка работы

В любом вашем репозитории в `.github/workflows/test.yml` укажите `runs-on: arc-runner-set`:

```yaml
jobs:
  test:
    runs-on: arc-runner-set
    steps:
      - uses: actions/checkout@v4
      - run: |
          echo "Тесты бегут на моем компьютере внутри Kubernetes!"
          uname -a
```

В момент запуска workflow вы увидите, как Kubernetes автоматически создает новый под:
```bash
kubectl get pods -n arc-runners -w
```
После завершения тестов под автоматически удалится.

---

## ❓ Нужен ли Docker внутри раннеров?

**Сразу определяться не нужно!**
- Сейчас по умолчанию включен легкий `helm/runners/values.yaml` (без Docker-демона). Он запускается за 2-3 секунды и кушает минимум ресурсов.
- **Если позже вам понадобится собирать Docker-образы** (`docker build`) или запускать контейнеры в тестах:
  Просто переключите в `apps/arc-runners.yaml` файл значений на `helm/runners/values-dind.yaml` (или создайте второй независимый ScaleSet с именем `runs-on: arc-runner-dind`).

---

## 🔍 Полезные команды диагностики

```bash
# Проверить статус контроллера
kubectl get pods -n arc-systems

# Проверить статус пула раннеров
kubectl get autoscalingrunnersets -n arc-runners

# Логи контроллера при проблемах с подключением к GitHub
kubectl logs -n arc-systems -l app.kubernetes.io/name=gha-runner-scale-set-controller -f
```
