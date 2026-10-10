
 Current state                                                                                                                                                                                
                                                                                                                                                                                              
 The codegen emits a single run() function for a whole stratified program (including recursive strata with delta scans and the semi-naive loop), and supports: Iterate (all 3 DataSource      
 variants), Filter with Cmp only, Insert, Let, HashJoin, Seq/Nop. Everything else is missing — and importantly, three gaps are silent wrong-results, not panics.                              
                                                                                                                                                                                              
 Gap analysis, by severity                                                                                                                                                                    
                                                                                                                                                                                              
 ### 1. Silent wrong results (worst — violates the fail-loudly policy we just established)                                                                                                    
                                                                                                                                                                                              
 - Op::GroupBy → emits nothing (crates/mangle-codegen/src/lib.rs, emit_op: Op::GroupBy { .. } => {}). Any program with aggregations (fn:sum, fn:count, fn:min, fn:max, fn:collect, float      
   variants) compiles to WASM that silently produces empty results. Ironically, the host protocol is already half-designed: IMP_SCAN_AGG_START exists, the Host trait has                     
   scan_aggregate_start(rel_id, desc: Vec<i32>), and the module even exports a memory "for aggregate descriptions" — the emission was just never written.                                     
 - fn:map:keys / fn:map:values / fn:struct:values emit compound_len instead of the actual keys/values (explicit TODO: proper keys/values extraction in emit_expr). Wrong value, not an error. 
 - Unknown-function catch-all in emit_expr drops args and pushes RefNull — same silent-null disease the condition catch-all had before your fix.                                              
 - emit_hash_join silently returns for non-Scan data sources (currently unreachable since the planner only emits that shape, but it's a latent trap).                                         
                                                                                                                                                                                              
 ### 2. Loud panics (correct but unsupported)                                                                                                                                                 
                                                                                                                                                                                              
 - Condition::Negation — plain Datalog negation !foo(X, Y). This is the single most fundamental missing feature: any program using negation at all fails WASM compilation.                    
 - Condition::Call — string builtin filters: :string:contains, :string:starts_with, :string:ends_with, :match_prefix.                                                                         
 - Condition::Not — our new negated builtins (!:list:member, !:lt, …).                                                                                                                        
 - Op::MatchField — :match_field(Struct, /Field, Var).                                                                                                                                        
 - Op::IterateList — :list:member(Elem, List) in binding mode.                                                                                                                                
                                                                                                                                                                                              
 ### 3. Missing Expr::Call functions (interpreter's eval_function has ~70, codegen ~26)                                                                                                       
                                                                                                                                                                                              
 - fn:list:append                                                                                                                                                                             
 - All 11 fn:duration:* (from_hours, from_seconds, hours, nanos, add, mult, parse, …)                                                                                                         
 - All 17 fn:time:* (now, year, month, format, parse_rfc3339, trunc, …) — note fn:time:now is non-deterministic, which deserves a semantic decision for compiled modules                      
                                                                                                                                                                                              
 ### 4. Prerequisites / plumbing (needed before the above)                                                                                                                                    
                                                                                                                                                                                              
 - collect_vars doesn't visit MatchField/IterateList — their bound vars never get locals, so emission can't work even once written.                                                           
 - Backend trait + Host trait + import-index constants all need new entries; note the import indices are hardcoded consts (IMP_* 0–42), so every addition renumbers. New host imports needed: 
     - negation check — variadic arity problem; the hash_join_push-style buffer protocol is the established pattern to copy                                                                   
     - string-predicate checks → i32                                                                                                                                                          
     - list iteration (compound_iter_start(externref) -> iter_id, reusing scan_next/get_col)                                                                                                  
     - field extraction with presence semantics (compound_get exists but there's no null/presence check to branch on)                                                                         
     - time/duration functions                                                                                                                                                                
     - aggregate-scan semantics (define the desc format: key columns + agg func + arg column)                                                                                                 
                                                                                                                                                                                              
 Suggested order of attack                                                                                                                                                                    
                                                                                                                                                                                              
 1. Quick consistency fix first (minutes): make the three silent paths loud — GroupBy arm, map:keys-family, unknown-function catch-all — so nothing silently lies while we work.              
 2. Condition::Negation — biggest parity win, unblocks all negation programs.                                                                                                                 
 3. Condition::Call + Condition::Not — one host-import family (str_* checks + i32.eqz for Not(Cmp)) covers both positive and negated builtins, mirroring eval_builtin_predicate.              
 4. MatchField + IterateList — together with collect_vars fix; both follow the existing scan-loop emission pattern.                                                                           
 5. GroupBy — most design work (host aggregate protocol), but the import is already stubbed.                                                                                                  
 6. Expr functions — mechanical host-import additions, biggest count, least risk.                                                                                                             
                                                                                                                                                                                              
 Want me to start with the consistency fixes and the negation support (items 1–2), or would you rather see a different prioritization?    

