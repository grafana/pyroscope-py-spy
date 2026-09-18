#!/env/bin/python
# -*- coding: utf-8 -*-
import time

# non-BMP identifier, so cpython stores this name as a kind=4 (UCS-4) string
def 𝕗𝕦𝕟𝕔𝕥𝕚𝕠𝕟𝟙(seconds):
    time.sleep(seconds)

if __name__ == "__main__":
    𝕗𝕦𝕟𝕔𝕥𝕚𝕠𝕟𝟙(100)
