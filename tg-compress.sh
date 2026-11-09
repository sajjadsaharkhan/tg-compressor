#!/usr/bin/env bash
# Compress a video into a Telegram-friendly MP4 (H.264 + AAC, yuv420p, faststart).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Built-in defaults, overridden by the config file, then by flags.
MODE="quality"
CRF=26
TARGET_MB=50
MAX_RES=720
PRESET="slow"
AUDIO_KBPS=128
CONFIG="$SCRIPT_DIR/tg-compress.conf"

usage() {
  cat <<EOF
Usage: $(basename "$0") [options] input.mp4 [more inputs...]

Options:
  -s MB       Size mode: fit output under MB megabytes (two-pass)
  -q CRF      Quality mode: fixed CRF (lower = better/larger, default $CRF)
  -r PIXELS   Cap shorter side to PIXELS (default $MAX_RES, 0 = original)
  -p PRESET   x264 preset (default $PRESET)
  -a KBPS     Audio bitrate (default $AUDIO_KBPS)
  -o FILE     Output file (only with a single input; default: <name>_tg.mp4)
  -c FILE     Config file (default: $CONFIG)
  -h          Show this help
EOF
}

# Load config first (a -c flag must be read before other flags apply).
for ((i = 1; i <= $#; i++)); do
  if [[ "${!i}" == "-c" ]]; then
    j=$((i + 1)); CONFIG="${!j:-}"
  fi
done
if [[ -f "$CONFIG" ]]; then
  # shellcheck source=/dev/null
  source "$CONFIG"
fi

OUTPUT=""
while getopts ":s:q:r:p:a:o:c:h" opt; do
  case "$opt" in
    s) MODE="size"; TARGET_MB="$OPTARG" ;;
    q) MODE="quality"; CRF="$OPTARG" ;;
    r) MAX_RES="$OPTARG" ;;
    p) PRESET="$OPTARG" ;;
    a) AUDIO_KBPS="$OPTARG" ;;
    o) OUTPUT="$OPTARG" ;;
    c) ;; # already handled
    h) usage; exit 0 ;;
    :) echo "Error: -$OPTARG needs a value" >&2; usage; exit 1 ;;
    \?) echo "Error: unknown option -$OPTARG" >&2; usage; exit 1 ;;
  esac
done
shift $((OPTIND - 1))

[[ $# -ge 1 ]] || { usage; exit 1; }
[[ -n "$OUTPUT" && $# -gt 1 ]] && { echo "Error: -o works with a single input only" >&2; exit 1; }
command -v ffmpeg >/dev/null && command -v ffprobe >/dev/null \
  || { echo "Error: ffmpeg/ffprobe not found (brew install ffmpeg)" >&2; exit 1; }

# Scale so the shorter side is at most MAX_RES; keep dimensions even (required by yuv420p).
if [[ "$MAX_RES" -gt 0 ]]; then
  VF="scale='if(gt(iw,ih),-2,min($MAX_RES,iw))':'if(gt(iw,ih),min($MAX_RES,ih),-2)',format=yuv420p"
else
  VF="scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p"
fi

compress() {
  local in="$1" out="$2"
  local has_audio
  has_audio="$(ffprobe -v error -select_streams a:0 -show_entries stream=index -of csv=p=0 "$in")"

  local audio_args=(-an)
  [[ -n "$has_audio" ]] && audio_args=(-c:a aac -b:a "${AUDIO_KBPS}k" -ac 2)

  local common=(-hide_banner -loglevel error -stats -y -i "$in"
    -map 0:v:0 -map "0:a:0?" -vf "$VF"
    -c:v libx264 -preset "$PRESET" -profile:v high -pix_fmt yuv420p)

  if [[ "$MODE" == "size" ]]; then
    local duration
    duration="$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$in")"
    # Total kbps budget with ~3% headroom for container overhead.
    local total_kbps video_kbps audio_kbps=0
    total_kbps="$(awk -v mb="$TARGET_MB" -v d="$duration" 'BEGIN { printf "%d", mb * 8192 * 0.97 / d }')"
    [[ -n "$has_audio" ]] && audio_kbps="$AUDIO_KBPS"
    video_kbps=$((total_kbps - audio_kbps))
    if (( video_kbps < 100 )); then
      echo "Error: ${TARGET_MB} MB is too small for a ${duration%.*}s video (would be ${video_kbps} kbps video)." >&2
      return 1
    fi
    echo "Size mode: target ${TARGET_MB} MB -> video ${video_kbps} kbps, audio ${audio_kbps} kbps"

    local passlog
    passlog="$(mktemp -d)/pass"
    echo "Pass 1/2..."
    ffmpeg "${common[@]}" -b:v "${video_kbps}k" -pass 1 -passlogfile "$passlog" -an -f mp4 /dev/null
    echo "Pass 2/2..."
    ffmpeg "${common[@]}" -b:v "${video_kbps}k" -maxrate "$((video_kbps * 3 / 2))k" -bufsize "$((video_kbps * 2))k" \
      -pass 2 -passlogfile "$passlog" "${audio_args[@]}" -movflags +faststart "$out"
    rm -rf "$(dirname "$passlog")"
  else
    echo "Quality mode: CRF $CRF"
    ffmpeg "${common[@]}" -crf "$CRF" "${audio_args[@]}" -movflags +faststart "$out"
  fi

  local in_size out_size
  in_size="$(du -h "$in" | cut -f1)"
  out_size="$(du -h "$out" | cut -f1)"
  echo "Done: $out ($in_size -> $out_size)"
}

status=0
for in in "$@"; do
  if [[ ! -f "$in" ]]; then
    echo "Skipping '$in': not a file" >&2; status=1; continue
  fi
  out="${OUTPUT:-${in%.*}_tg.mp4}"
  echo "==> $in"
  compress "$in" "$out" || status=1
done
exit $status
