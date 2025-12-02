#!/bin/bash

# Hot reload development script for tt-devpro

set -e

case "$1" in
  start)
    echo "Starting development environment with hot reload..."
    docker compose -f docker-compose.dev.yml up --build
    ;;
  stop)
    echo "Stopping development environment..."
    docker compose -f docker-compose.dev.yml down
    ;;
  restart)
    echo "Restarting development environment..."
    docker compose -f docker-compose.dev.yml restart
    ;;
  logs)
    docker compose -f docker-compose.dev.yml logs -f
    ;;
  clean)
    echo "Cleaning up development environment and cache..."
    docker compose -f docker-compose.dev.yml down -v
    ;;
  *)
    echo "Usage: ./dev.sh {start|stop|restart|logs|clean}"
    echo ""
    echo "Commands:"
    echo "  start   - Start dev environment with hot reload (default)"
    echo "  stop    - Stop dev environment"
    echo "  restart - Restart dev environment"
    echo "  logs    - Show logs"
    echo "  clean   - Stop and remove volumes (Gradle cache)"
    exit 1
    ;;
esac
