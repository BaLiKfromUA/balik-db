Machine:
```
  os:     Linux 6.8.0-124-generic x86_64
  cpu:    AMD Ryzen 9 5900HS with Radeon Graphics (16 cores)
  memory: 31494 MB
```

Building release binary...
```
Generating ~1GB of data into '/tmp/tmp.p17G6cnYXp/db' (one-time)...
Generated 3184713 rows into table 'bench' at '/tmp/tmp.p17G6cnYXp/db' in 9.0s (954.2 MiB on disk)
```

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
```

Physical Plan:
```
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
```

Physical Plan:
```
ProjectionExec [id, sort_key]
  TopKExec [sort_key] 20
    FilterExec [filter_a > 500000 AND filter_b < 500000]
      TableScanExec bench [id, sort_key, filter_a, filter_b] prune=[filter_a > 500000, filter_b < 500000]
```

Timing (lower is better):
```
Benchmark 1: no-optimize
  Time (mean ± σ):      3.217 s ±  0.027 s    [User: 1.718 s, System: 1.499 s]
  Range (min … max):    3.170 s …  3.257 s    10 runs
 
Benchmark 2: optimize
  Time (mean ± σ):     430.4 ms ±   2.7 ms    [User: 150.5 ms, System: 279.7 ms]
  Range (min … max):   428.3 ms … 437.6 ms    10 runs
```

Summary
```
  'optimize' ran
    7.48 ± 0.08 times faster than 'no-optimize'
```