cd "$(dirname "$0")"
if [ ! -d target ]; then
    mkdir target
    uid=$(dumpsys package com.termux | grep appId | awk 'NR==1{print $1}' | cut -d '=' -f2)
    chown -R $uid:$uid ./target
    chmod -R 0755 ./target
fi
name=$(basename "$PWD")
rm -f target/"$name"_all_code.txt
{
    for i in $(fd -t f rs) Cargo.toml ./*/Cargo.toml; do
        # i="$(realpath $i)"
        echo "这是$i: "
        cat "$i"
        printf '\n--------------\n\n'
    done
} > target/"$name"_all_code.txt
