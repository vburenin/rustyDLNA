#!/usr/bin/env python3
"""Deterministic structural media controls; no decoder or real media needed."""
from pathlib import Path
import struct
root=Path(__file__).resolve().parent
def u(n): return struct.pack('>I',n)
def box(k,p): return u(8+len(p))+k+p
def full(k,flags,p): return box(k,u(flags)+p)
def out(folder,name,b):
 p=root/folder/name; p.parent.mkdir(parents=True,exist_ok=True); p.write_bytes(b)
# Three 1-second fragments, sync/non-sync/sync: two native segments (2s,1s).
tkhd=b'\0'*12+u(1); mdhd=b'\0'*12+u(1000); hdlr=b'\0'*8+b'vide'
trak=box(b'trak',box(b'tkhd',tkhd)+box(b'mdia',box(b'mdhd',mdhd)+box(b'hdlr',hdlr)))
init=box(b'ftyp',b'iso6')+box(b'moov',trak+box(b'mvex',box(b'trex',u(0)+u(1)+u(0)+u(1000)+u(0)+u(0))))
hls=init; spans=[]
for i,flags in enumerate([0,65536,0]):
 frag=box(b'moof',box(b'traf',full(b'tfdt',0,u(i*1000))+full(b'tfhd',0,u(1))+full(b'trun',4,u(1)+u(flags))))+box(b'mdat',bytes([1,2,3,4]))
 spans.append((len(hls),len(frag),1.0)); hls+=frag
out('seeds/mp4_index','three-fragments.mp4',hls)
out('regressions/mp4_index','truncated-fragment.mp4',hls[:-1])
out('regressions/mp4_index','box-size-underflow.mp4',u(7)+b'moov')
print('HLS',len(init),spans)
# Two VFR samples with signed composition offsets, edit bytes and one chunk.
video=b''; compact=b''; hevc=b''; sizes=[]; new_sizes=[]
for ordinal in [1,2]:
 aud=bytes([70,1,16]); vcl=bytes([2,1,128,ordinal]); eos=bytes([72,1]); rpu=bytes([124,1,64,ordinal,99,99]); el=bytes([126,1,42,ordinal]); replacement=bytes([124,1,64,ordinal])
 sample=b''.join(u(len(n))+n for n in [aud,vcl,el,eos,rpu]); video+=sample; sizes.append(len(sample))
 rewritten=b''.join(u(len(n))+n for n in [aud,vcl,eos,replacement]); compact+=rewritten; new_sizes.append(len(rewritten))
 hevc+=b''.join(b'\0\0\0\1'+n for n in [aud,bytes([64,1,42]),vcl,eos,replacement])
mp4=box(b'ftyp',b'fixture '); chunk=len(mp4)+8; mp4+=box(b'mdat',video)
hvcc=bytearray(23); hvcc[0]=1; hvcc[21]=3
entry=box(b'hvc1',bytes(78)+box(b'hvcC',hvcc)+box(b'dvcC',bytes(24)))
stbl=full(b'stsd',0,u(1)+entry)+full(b'stsz',0,u(0)+u(2)+b''.join(u(s) for s in sizes))+full(b'stco',0,u(1)+u(chunk))+full(b'stsc',0,b''.join(u(s) for s in [1,1,2,1]))+full(b'stts',0,b''.join(u(s) for s in [2,1,1000,1,3000]))+full(b'ctts',0x01000000,u(2)+u(1)+struct.pack('>i',-100)+u(1)+u(300))
mp4+=box(b'moov',box(b'trak',box(b'edts',box(b'elst',b'unchanged source edit list'))+box(b'mdia',box(b'minf',box(b'stbl',stbl)))))
expected=bytearray(mp4); expected[chunk:chunk+len(compact)]=compact
at=mp4.index(b'stsz')+16; expected[at:at+8]=b''.join(u(s) for s in new_sizes)
at=mp4.index(b'dvcC'); expected[at:at+4]=b'free'
packed=u(len(mp4))+mp4+hevc
out('seeds/profile8_rewrite','two-vfr-samples.bin',packed)
out('expected','two-vfr-samples.mp4',expected)
bad=bytearray(packed); at=4+mp4.index(b'stco')+12; bad[at:at+4]=u(0xffffffff)
out('regressions/profile8_rewrite','chunk-outside-mdat.bin',bad)
bad=bytearray(packed); at=4+len(mp4)+hevc.index(bytes([2,1,128,1]))+3; bad[at]=77
out('regressions/profile8_rewrite','mismatched-base-layer.bin',bad)
print('Profile8',len(mp4),len(hevc),sizes,new_sizes)

# Unknown metadata siblings stay opaque, even when their type has meaning elsewhere.
extra=box(b'stsz',b'')+box(b'moov',b'opaque child bytes')
variant=bytearray(mp4)
at=mp4.index(b'moov')-4
variant[at:at+4]=u(len(mp4)-at+len(extra))
variant.extend(extra)
out('seeds/profile8_rewrite','opaque-moov-children.bin',u(len(variant))+variant+hevc)
