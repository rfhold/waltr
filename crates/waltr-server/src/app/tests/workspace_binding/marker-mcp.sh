while IFS= read -r request; do
    id=${request#*'"id":'}
    id=${id%%,*}
    id=${id%%\}*}
    case "$request" in
        *'"method":"server/discover"'*)
            result='{"supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"}' ;;
        *'"method":"tools/list"'*)
            result='{"tools":[{"name":"read.marker","inputSchema":{"type":"object"}}]}' ;;
        *'"method":"tools/call"'*)
            if [ "$1" = exporter ]; then
                marker=
                IFS= read -r marker < sentinel.txt || [ -n "$marker" ] || exit 1
            else
                marker='destination launch definition ran'
            fi
            result=$(printf '{"content":[{"type":"text","text":"%s"}]}' "$marker") ;;
        *) continue ;;
    esac
    printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
