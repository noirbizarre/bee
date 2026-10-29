#!/usr/bin/env bash
# A real stdio peer: no shell executes model input, and marker files expose
# lifecycle ordering without depending on timing or a test-only runtime.
printf '%s\n' "$$" >> "$MCP_MARKER"
printf '%s\n' 'private-child-stderr-sentinel' >&2
while IFS= read -r line; do
    [[ "$line" =~ \"id\":([0-9]+) ]] || continue
    id="${BASH_REMATCH[1]}"
    case "$line" in
        *'"method":"initialize"'*)
            if [[ "$MCP_INIT_BLOCK" == yes ]]; then sleep 60 & wait; fi
            [[ "$line" =~ \"protocolVersion\":\"([^\"]+)\" ]] || exit 1
            version="${BASH_REMATCH[1]}"
            printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"%s","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}\n' "$id" "$version"
            ;;
        *'"method":"tools/list"'*)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo.original","description":"Fixture echo","inputSchema":{"type":"object"}}]}}\n' "$id"
            ;;
        *'"method":"tools/call"'*)
            printf '%s\n' call >> "$MCP_CALLS"
            if [[ "$MCP_BLOCK" == yes ]]; then
                # A descendant retaining inherited pipes exercises group/job cleanup.
                sleep 60 &
                printf '%s\n' "$!" >> "$MCP_CALLS"
                wait
            else
                printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[],"structuredContent":{"nested":[1,true,null]},"extension":42}}\n' "$id"
            fi
            ;;
        *) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"unsupported"}}\n' "$id" ;;
    esac
done
