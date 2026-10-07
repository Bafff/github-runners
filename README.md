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
