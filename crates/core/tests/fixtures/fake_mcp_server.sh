#!/bin/sh
# A stand-in MCP stdio server for mcp_probe tests. No network.
# FAKE_MODE: ok | silent | error | noisy | paged
mode="${FAKE_MODE:-ok}"
if [ "$mode" = silent ]; then
  sleep 600 &
  sleep 600
  exit 0
fi
if [ "$mode" = noisy ]; then
  echo "Starting fake server v1"
fi
tools='{"name":"browser_navigate"},{"name":"browser_take_screenshot"}'
if [ "$DEMO_TOKEN" = s3cret-value ]; then
  tools="$tools,{\"name\":\"token_ok\"}"
fi
echo "server log: token is $DEMO_TOKEN" >&2
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      if [ "$mode" = error ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32603,"message":"init failed: %s"}}\n' "$id" "$DEMO_TOKEN"
      else
        printf '{"jsonrpc":"2.0","method":"notifications/message","params":{}}\n'
        printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}}}\n' "$id"
      fi
      ;;
    *'"method":"tools/list"'*)
      if [ "$mode" = paged ]; then
        case "$line" in
          *'"cursor":"p2"'*)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"page_two"}]}}\n' "$id" ;;
          *)
            printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"page_one"}],"nextCursor":"p2"}}\n' "$id" ;;
        esac
      else
        printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[%s]}}\n' "$id" "$tools"
      fi
      ;;
  esac
done
