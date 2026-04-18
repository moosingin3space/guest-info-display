#!/bin/bash

flatpak run --command=flathub-build org.flatpak.Builder ./xyz.mooshq.GuestInfoDisplay.json "$@"
