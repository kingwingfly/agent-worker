#!/usr/bin/env bash
# Stand-in for `claude -p --input-format stream-json --output-format stream-json`.
# Records its arguments, then answers each user message with a canned event sequence.
# Keywords in the message select a behaviour: pdf, crash, slow, stale, linger, sluggish.

printf '%s\n' "$@" > "$CLAUDE_CONFIG_DIR/args"

trap 'kill $! 2>/dev/null
      echo "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true}"
      exit 130' INT

while IFS= read -r line; do
  text=$(sed -n 's/.*"text":"\([^"]*\)".*/\1/p' <<<"$line")
  uuid=$(sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p' <<<"$line")
  echo '{"type":"system","subtype":"init"}'
  # A result left over from something Claude did on its own, before it reads the message.
  [[ $text == *stale* ]] && echo '{"type":"result","subtype":"success","is_error":false,"result":""}'
  # Slow to pick the message up, so an interrupt can arrive before the acknowledgment.
  [[ $text == *sluggish* ]] && { sleep 5 & wait $!; }
  echo "{\"type\":\"user\",\"isReplay\":true,\"uuid\":\"$uuid\"}"
  case "$text" in
    *pdf*) mkdir -p out/reports && printf PDF > out/reports/summary.pdf ;;
    *crash*) echo "simulated failure" >&2; exit 3 ;;
    *slow*) sleep "${FAKE_CLAUDE_SLOW_SECS:-30}" & wait $! ;;
  esac
  echo "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"echo: $text\"}]}}"
  echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"echo: $text\",\"usage\":{\"input_tokens\":3,\"output_tokens\":5,\"service_tier\":\"standard\"}}"
  # Exit a moment after answering, without reading whatever was sent in between.
  [[ $text == *linger* ]] && sleep 0.3 && exit 0
done
