#!/env/bin/python
# -*- coding: utf-8 -*-
import time

# CJK ext B, not mathematical alphanumerics: identifiers are NFKC-normalized while parsing,
# so a name like 𝕗𝕦𝕟𝕔 would reach us as plain ascii and stop testing the kind=4 path
def 𠀀𠀁𠀂𠀋(seconds):
    time.sleep(seconds)

if __name__ == "__main__":
    𠀀𠀁𠀂𠀋(100)
