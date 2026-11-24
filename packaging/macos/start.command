#!/bin/sh
# Start the server with persistent data beside this launcher.
cd "$(dirname "$0")" || exit 1
FMP_DATA_DIR="$(pwd)/data"
export FMP_DATA_DIR
if [ -z "${FMP_LISTEN:-}" ]; then
    FMP_LISTEN="127.0.0.1:8317"
    export FMP_LISTEN
fi
echo "fakemodelproxy"
echo "Console: http://$FMP_LISTEN/"
echo "Keep this window open. Press Ctrl+C to stop."
./fakemodelproxy
status=$?
if [ "$status" -ne 0 ]; then
    echo "Server exited with code $status."
    printf "Press Enter to close."
    read -r _
fi
exit "$status"
