import sys

pre_lines = []
uses = []
post_lines = []

with open(sys.argv[1]) as f:
    for line in f:
        if line.startswith('use '):
            uses.append(line + '\n')
            while not uses[-1].strip().endswith(';'):
                uses[-1] += next(f) + '\n'
        elif not pre_lines:
            pre_lines.append(line)
        else:
            post_lines.append(line)


def order(line):
    if line.startswith('use std::'):
        return '0000000000'
    if line.startswith('use crate::'):
        return 'zzzzzzzzzz'
    return line

uses.sort(key=order)

with open(sys.argv[1], 'w') as f:
    for chunk in [pre_lines, uses, post_lines]:
        for line in chunk:
            f.write(line)
