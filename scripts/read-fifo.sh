#!/bin/bash

while true
do
    if read line <$1; then
        if [[ "$line" == 'quit' ]]; then
            break
        fi
        echo $line
    fi
done

echo "Reader exiting"
