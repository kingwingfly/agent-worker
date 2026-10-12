#!/usr/bin/env bash
# Stand-in for `claude -p --input-format stream-json --output-format stream-json`.
# Records its arguments, then answers each user message with a canned event sequence.
# Keywords in the message select a behaviour: pdf, crash, slow, stale, linger, sluggish,
# typed. A control request is answered, and the effort it sets is recorded.

printf '%s\n' "$@" > "$CLAUDE_CONFIG_DIR/args"

# One piece of a response, as `--include-partial-messages` prints them: $1 is the model's
# event and $2 the tool call of the sub-agent it comes from, if it comes from one.
piece() {
  echo "{\"type\":\"stream_event\",\"event\":$1,\"parent_tool_use_id\":${2:-null}}"
}
# A piece that carries text ($1), or with $2 another kind of delta under the same field.
delta() {
  piece "{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"${2:-text}_delta\",\"text\":\"$1\"}}" "$3"
}

trap 'kill $! 2>/dev/null
      echo "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true}"
      exit 130' INT

while IFS= read -r line; do
  if [[ $line == *'"type":"control_request"'* ]]; then
    sed -n 's/.*"effortLevel":"\{0,1\}\([a-z]*\).*/\1/p' <<<"$line" >> "$CLAUDE_CONFIG_DIR/efforts"
    request_id=$(sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p' <<<"$line")
    echo "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$request_id\"}}"
    continue
  fi
  text=$(sed -n 's/.*"text":"\([^"]*\)".*/\1/p' <<<"$line")
  uuid=$(sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p' <<<"$line")
  echo '{"type":"system","subtype":"init"}'
  # A result left over from something Claude did on its own, before it reads the message.
  [[ $text == *stale* ]] && echo '{"type":"result","subtype":"success","is_error":false,"result":""}'
  # So are pieces of a response that was on its way before.
  [[ $text == *typed* ]] && delta "left over "
  # Slow to pick the message up, so an interrupt can arrive before the acknowledgment.
  [[ $text == *sluggish* ]] && { sleep 5 & wait $!; }
  echo "{\"type\":\"user\",\"isReplay\":true,\"uuid\":\"$uuid\"}"
  case "$text" in
    *pdf*) mkdir -p out/reports && printf PDF > out/reports/summary.pdf ;;
    *crash*) echo "simulated failure" >&2; exit 3 ;;
    *slow*) sleep "${FAKE_CLAUDE_SLOW_SECS:-30}" & wait $! ;;
    # The answer in three pieces, the last one late, among pieces that are not its text.
    *typed*)
      piece '{"type":"message_start","message":{"content":[]}}'
      delta "thinking it over" thinking
      delta "a sub-agent at work" text '"toolu_1"'
      piece '{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}'
      delta "echo: "
      delta "${text:0:3}"
      sleep 0.9
      delta "${text:3}" ;;
  esac
  echo "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"echo: $text\"}]}}"
  echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"echo: $text\",\"usage\":{\"input_tokens\":3,\"output_tokens\":5,\"service_tier\":\"standard\"}}"
  # Exit a moment after answering, without reading whatever was sent in between.
  [[ $text == *linger* ]] && sleep 0.3 && exit 0
done
