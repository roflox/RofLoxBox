#!/usr/bin/env bash
# Like top, for RofLoxBox only: the app (rofloxbox-app / RofLoxBox, the
# RofLoxBox CLI) and everything it started (the demucs stem worker…), every
# few seconds, busiest first. Ctrl-C to stop.
#
#   scripts/top-app.sh        refresh every 2 s
#   scripts/top-app.sh 5      every 5 s
#   scripts/top-app.sh -1    once, then quit

interval="${1:-2}"
# What counts as the app: its binaries and its stem worker.
pattern='rofloxbox-app|RofLoxBox|/rofloxbox( |$)|demucs_worker'

# The app's processes and all their children, grandchildren…
pids() {
    local found queue pid kids
    found=$(pgrep -f "$pattern" | grep -vx "$$")
    queue="$found"
    while [ -n "$queue" ]; do
        kids=""
        for pid in $queue; do
            kids="$kids $(pgrep -P "$pid")"
        done
        queue=$(echo "$kids" | xargs)
        found="$found $queue"
    done
    echo "$found" | tr ' ' '\n' | grep -v '^$' | sort -un | tr '\n' ',' | sed 's/,$//'
}

show() {
    local list
    list=$(pids)
    if [ "$interval" = "-1" ]; then
        printf 'RofLoxBox processes   %s\n\n' "$(date '+%H:%M:%S')"
    else
        printf 'RofLoxBox processes   %s   (every %ss, Ctrl-C to stop)\n\n' "$(date '+%H:%M:%S')" "$interval"
    fi
    if [ -z "$list" ]; then
        echo "  not running"
        return
    fi
    # CPU % is of one core; RSS is memory in use.
    ps -o pid=PID,ppid=PARENT,%cpu=CPU%,%mem=MEM%,rss=,etime=UPTIME,command=COMMAND -p "$list" -r |
        awk 'NR == 1 { printf "%-7s %-7s %6s %6s %9s %12s  %s\n", "PID", "PARENT", "CPU%", "MEM%", "MEMORY", "UPTIME", "COMMAND"; next }
             { rss = $5; mem = rss > 1048576 ? sprintf("%.1f GB", rss / 1048576) : sprintf("%.0f MB", rss / 1024)
               cmd = ""; for (i = 7; i <= NF; i++) cmd = cmd (i > 7 ? " " : "") $i
               if (length(cmd) > 110) cmd = substr(cmd, 1, 107) "..."
               printf "%-7s %-7s %6s %6s %9s %12s  %s\n", $1, $2, $3, $4, mem, $6, cmd }'
}

if [ "$interval" = "-1" ]; then
    show
    exit 0
fi
trap 'tput cnorm 2>/dev/null; exit 0' INT TERM
tput civis 2>/dev/null
while true; do
    out=$(show)
    clear
    echo "$out"
    sleep "$interval"
done
