#!/bin/bash

set -e

ufw default deny incoming
ufw default allow outgoing
ufw allow 2222/tcp
ufw --force enable
