import psutil,time,json,subprocess
p=next(p for p in psutil.process_iter(['name','cmdline']) if p.info['name']=='envoy' and '--mode' not in p.info['cmdline'])
f=open('/tmp/envoy-spike/memory.jsonl','w');t=time.monotonic();baseline=p.memory_info().rss
cmd=subprocess.Popen(['bash','/workspace/flotilla/docs/research/3593-envoy-credential-proxy/git-check.sh','push'],stdout=open('/tmp/envoy-spike/git-large.log','w'),stderr=subprocess.STDOUT)
peak=baseline;n=0
while cmd.poll() is None:
 rss=p.memory_info().rss;peak=max(peak,rss);n+=1;f.write(json.dumps({'seconds':time.monotonic()-t,'rss':rss})+'\n');f.flush();time.sleep(.02)
print(json.dumps({'exit':cmd.returncode,'baseline_rss':baseline,'peak_rss':peak,'delta':peak-baseline,'elapsed':time.monotonic()-t,'samples':n}))
