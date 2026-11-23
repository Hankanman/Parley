import json, subprocess, sys, time, statistics
helper, model, runs = sys.argv[1], sys.argv[2], int(sys.argv[3])
SEQ = [32768, 16384, 8192, 8192, 16384, 16384, 8192, 16384]
PROMPT = "<start_of_turn>user\nSummarise in one sentence: the team agreed to ship on Friday.<end_of_turn>\n<start_of_turn>model\n"
totals, per = [], [[] for _ in SEQ]
for _ in range(runs):
    p = subprocess.Popen([helper], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
    t0 = time.perf_counter()
    for i, c in enumerate(SEQ):
        t = time.perf_counter()
        p.stdin.write(json.dumps({"type":"generate","prompt":PROMPT,"max_tokens":16,"context_size":c,"model_path":model,"temperature":0.0})+"\n"); p.stdin.flush()
        while True:
            r = json.loads(p.stdout.readline())
            if r["type"] == "response":
                if r.get("error"): sys.exit(f"error: {r['error']}")
                break
        per[i].append(time.perf_counter() - t)
    totals.append(time.perf_counter() - t0)
    p.stdin.write('{"type":"shutdown"}\n'); p.stdin.flush(); p.wait()
print(f"total {statistics.median(totals):6.2f}s (median of {runs})  per request: " + " ".join(f"{c//1024}k={statistics.median(x):.2f}" for c, x in zip(SEQ, per)))
