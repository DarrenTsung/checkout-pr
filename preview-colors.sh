#!/usr/bin/env bash
# Preview iTerm2 background colors - press Enter to cycle, Ctrl+C to exit

colors=(
    "1e2233:soft navy"
    "1e2828:soft sage"
    "2d1f2d:dusty plum"
    "1f2d2d:seafoam"
    "2b2433:lavender"
    "33261f:warm taupe"
    "1f2b33:powder blue"
    "2d2626:dusty rose"
    "262d26:soft mint"
    "332b1f:soft peach"
    "261f2d:soft violet"
    "1f332b:soft teal"
)

reset_bg() {
    printf '\033]111\007'
}

trap reset_bg EXIT

i=0
while true; do
    IFS=':' read -r hex name <<< "${colors[$i]}"
    printf '\033]1337;SetColors=bg=%s\007' "$hex"
    echo "[$((i+1))/${#colors[@]}] $name (#$hex) - Press Enter for next, q to quit"
    read -r key
    [[ "$key" == "q" ]] && break
    i=$(( (i + 1) % ${#colors[@]} ))
done
