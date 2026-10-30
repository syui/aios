#!/bin/bash

set -e

ufw default deny incoming
ufw default allow outgoing
ufw allow 22/tcp
ufw --force enable
