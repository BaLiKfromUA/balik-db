Machine:
```
  os:     Darwin 25.5.0 arm64
  cpu:    Apple M3 Pro (12 cores)
  memory: 36864 MB
```

Building release binary...
Generating ~1GB of data into '/var/folders/yg/qcvq6s713yl5bjm4h17bjwpw0000gp/T/tmp.HPXbUdwzlA/db' (one-time)...
Generated 3184713 rows into table 'bench' at '/var/folders/yg/qcvq6s713yl5bjm4h17bjwpw0000gp/T/tmp.HPXbUdwzlA/db' in 16.7s (954.2 MiB on disk)

Generated table:
```
column_name      | type         | nullable
-----------------+--------------+---------
id               | INT          | NO      
sort_key         | INT          | NO      
filter_a         | INT          | NO      
filter_b         | INT          | NO      
payload1         | TEXT         | NO      
payload2         | TEXT         | NO      
payload3         | TEXT         | NO      
# table_id       | 1            |         
# storage        | column-store |         
# row_group_size | 8192         |         
```

Query:
```sql
SELECT id, sort_key FROM bench WHERE filter_a > 500000 AND filter_b < 500000 ORDER BY sort_key LIMIT 20
```

Plan WITHOUT optimization:
```
Logical Plan:
Projection [id, sort_key]
  Limit 20
    Sort [sort_key]
      Filter [filter_a > 500000 AND filter_b < 500000]
        Scan bench

Physical Plan:
ProjectionExec [id, sort_key]
  LimitExec 20
    SortExec [sort_key]
      FilterExec [filter_a > 500000 AND filter_b < 500000]
        TableScanExec bench prune=[filter_a > 500000, filter_b < 500000]
```

Plan WITH optimization:
```
Logical Plan:
Projection [id, sort_key]
  TopK [sort_key] 20
    Filter [filter_a > 500000 AND filter_b < 500000]
      Scan bench [id, sort_key, filter_a, filter_b]

Physical Plan:
ProjectionExec [id, sort_key]
  TopKExec [sort_key] 20
    FilterExec [filter_a > 500000 AND filter_b < 500000]
      TableScanExec bench [id, sort_key, filter_a, filter_b] prune=[filter_a > 500000, filter_b < 500000]
```

Timing (lower is better):
```
Benchmark 1: no-optimize
  Time (mean ± σ):      1.476 s ±  0.031 s    [User: 0.917 s, System: 0.284 s]
  Range (min … max):    1.436 s …  1.522 s    10 runs
 
Benchmark 2: optimize
  Time (mean ± σ):     367.5 ms ±  21.2 ms    [User: 97.9 ms, System: 112.3 ms]
  Range (min … max):   330.5 ms … 404.9 ms    10 runs
 
Summary
  optimize ran
    4.02 ± 0.25 times faster than no-optimize
```
