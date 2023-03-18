# Changelog

## 0.2.2
- Improvement: Correct use for optional configs
- Improvement: Check zap request amount matches invoice amount

## 0.2.0
- Fix: Increment pay index count for non zaps [(@denis2342)](https://github.com/denis2342) 
- Improvement: Add option to config for path of last pay index tip
- Refactor: use filter map on relays to avoid second loop

## 0.1.2 
- Improvement: Add preimage tag to zap note

## 0.1.1
- Improvement: Save paid invoice index tip to file. Restart from saved tip
- Fix: Use invoice description for zap request description tag [(@denis2342)](https://github.com/denis2342)