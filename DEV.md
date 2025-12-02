# Development Mode with Hot Reload

## Quick Start

```bash
./dev.sh start
```

Приложение автоматически перезапустится при изменении `.kt` файлов в `src/`.

## Commands

- `./dev.sh start` - запустить с hot reload
- `./dev.sh stop` - остановить
- `./dev.sh restart` - перезапустить
- `./dev.sh logs` - посмотреть логи
- `./dev.sh clean` - очистить все (включая Gradle cache)

## How It Works

1. **Gradle Continuous Build** - `--continuous` отслеживает изменения
2. **Volume Mounts** - исходники монтируются read-only
3. **Gradle Cache** - зависимости кэшируются в Docker volume
4. **Auto Rebuild** - при изменении файлов Gradle пересобирает и перезапускает

## Files

- `Dockerfile.dev` - образ для разработки
- `docker-compose.dev.yml` - конфигурация с volumes
- `dev.sh` - удобный скрипт запуска
- `Dockerfile` - production build (без изменений)

## Notes

- Первый запуск может быть медленным (скачивание зависимостей)
- Последующие запуски быстрее благодаря кэшу
- Изменения применяются через 1-2 секунды после сохранения
