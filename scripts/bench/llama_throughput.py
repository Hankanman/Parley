import json, subprocess, sys, time, statistics, random
helper, model = sys.argv[1], sys.argv[2]
random.seed(7)
names = ["Ross", "Lee", "Tom", "Priya", "Dave"]
topics = ["the Q4 budget", "the migration plan", "supplier pricing", "the hiring freeze", "customer churn", "the release date", "security review findings"]
verbs = ["thinks we should revisit", "wants a decision on", "raised concerns about", "will send numbers on", "suggested we defer", "agreed to own"]
def transcript(n_sent):
    return "\n".join(f"[{i//4:02d}:{(i*13)%60:02d}] {random.choice(names)}: I {random.choice(['think','reckon','feel'])} {random.choice(names)} {random.choice(verbs)} {random.choice(topics)}, and we need to be clear about the next steps before Friday." for i in range(n_sent))
def prompt(n): return f"<start_of_turn>user\nWrite a detailed meeting summary of about 500 words with decisions and action items.\n\n{transcript(n)}<end_of_turn>\n<start_of_turn>model\n"
p = subprocess.Popen([helper], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
def run(text, ctx, gen=400):
    t0 = time.perf_counter(); first = None
    p.stdin.write(json.dumps({"type":"generate","prompt":text,"max_tokens":gen,"context_size":ctx,"model_path":model,"temperature":0.0})+"\n"); p.stdin.flush()
    while True:
        r = json.loads(p.stdout.readline())
        if r["type"] == "chunk" and first is None: first = time.perf_counter() - t0
        if r["type"] == "response":
            if r.get("error"): sys.exit(r["error"])
            return first or 0.0, time.perf_counter() - t0, len(r["text"])
out = []
for n, ctx, label in [(90, 16384, "~3k tok"), (360, 32768, "~12k tok")]:
    text = prompt(n)
    run(text, ctx, 8)  # warm-up: load model and context at this size
    res = [run(text, ctx) for _ in range(2)]
    ttft = statistics.median(r[0] for r in res); tot = statistics.median(r[1] for r in res)
    out.append(f"{label}: prompt {ttft:5.2f}s  generate {tot-ttft:5.2f}s  total {tot:5.2f}s ({res[0][2]} chars)")
p.stdin.write('{"type":"shutdown"}\n'); p.stdin.flush(); p.wait()
print(" | ".join(out))
