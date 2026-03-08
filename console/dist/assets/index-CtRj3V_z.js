var Vi=Object.defineProperty;var io=e=>{throw TypeError(e)};var qi=(e,t,r)=>t in e?Vi(e,t,{enumerable:!0,configurable:!0,writable:!0,value:r}):e[t]=r;var Nr=(e,t,r)=>qi(e,typeof t!="symbol"?t+"":t,r),fs=(e,t,r)=>t.has(e)||io("Cannot "+r);var F=(e,t,r)=>(fs(e,t,"read from private field"),r?r.call(e):t.get(e)),Ze=(e,t,r)=>t.has(e)?io("Cannot add the same private member more than once"):t instanceof WeakSet?t.add(e):t.set(e,r),Ue=(e,t,r,n)=>(fs(e,t,"write to private field"),n?n.call(e,r):t.set(e,r),r),Qt=(e,t,r)=>(fs(e,t,"access private method"),r);(function(){const t=document.createElement("link").relList;if(t&&t.supports&&t.supports("modulepreload"))return;for(const s of document.querySelectorAll('link[rel="modulepreload"]'))n(s);new MutationObserver(s=>{for(const i of s)if(i.type==="childList")for(const l of i.addedNodes)l.tagName==="LINK"&&l.rel==="modulepreload"&&n(l)}).observe(document,{childList:!0,subtree:!0});function r(s){const i={};return s.integrity&&(i.integrity=s.integrity),s.referrerPolicy&&(i.referrerPolicy=s.referrerPolicy),s.crossOrigin==="use-credentials"?i.credentials="include":s.crossOrigin==="anonymous"?i.credentials="omit":i.credentials="same-origin",i}function n(s){if(s.ep)return;s.ep=!0;const i=r(s);fetch(s.href,i)}})();const xs=!1;var Vs=Array.isArray,Bi=Array.prototype.indexOf,rn=Array.prototype.includes,as=Array.from,Ki=Object.defineProperty,ya=Object.getOwnPropertyDescriptor,Wi=Object.getOwnPropertyDescriptors,Ji=Object.prototype,Gi=Array.prototype,jo=Object.getPrototypeOf,lo=Object.isExtensible;function Ka(e){return typeof e=="function"}const Me=()=>{};function Qi(e){for(var t=0;t<e.length;t++)e[t]()}function Ho(){var e,t,r=new Promise((n,s)=>{e=n,t=s});return{promise:r,resolve:e,reject:t}}function Yi(e,t){if(Array.isArray(e))return e;if(!(Symbol.iterator in e))return Array.from(e);const r=[];for(const n of e)if(r.push(n),r.length===t)break;return r}const cr=2,cn=4,an=8,ns=1<<24,la=16,Hr=32,za=64,ks=128,Pr=512,or=1024,ir=2048,jr=4096,gr=8192,ea=16384,Va=32768,ta=65536,co=1<<17,Xi=1<<18,un=1<<19,Zi=1<<20,Yr=1<<25,ja=65536,ws=1<<21,qs=1<<22,_a=1<<23,xa=Symbol("$state"),Uo=Symbol("legacy props"),el=Symbol(""),Ea=new class extends Error{constructor(){super(...arguments);Nr(this,"name","StaleReactionError");Nr(this,"message","The reaction that called `getAbortSignal()` was re-run or destroyed")}};var Io;const Bs=!!((Io=globalThis.document)!=null&&Io.contentType)&&globalThis.document.contentType.includes("xml");function zo(e){throw new Error("https://svelte.dev/e/lifecycle_outside_component")}function tl(){throw new Error("https://svelte.dev/e/async_derived_orphan")}function rl(e,t,r){throw new Error("https://svelte.dev/e/each_key_duplicate")}function al(e){throw new Error("https://svelte.dev/e/effect_in_teardown")}function nl(){throw new Error("https://svelte.dev/e/effect_in_unowned_derived")}function sl(e){throw new Error("https://svelte.dev/e/effect_orphan")}function ol(){throw new Error("https://svelte.dev/e/effect_update_depth_exceeded")}function il(e){throw new Error("https://svelte.dev/e/props_invalid_value")}function ll(){throw new Error("https://svelte.dev/e/state_descriptors_fixed")}function dl(){throw new Error("https://svelte.dev/e/state_prototype_fixed")}function cl(){throw new Error("https://svelte.dev/e/state_unsafe_mutation")}function ul(){throw new Error("https://svelte.dev/e/svelte_boundary_reset_onerror")}const fl=1,vl=2,Vo=4,gl=8,pl=16,hl=1,ml=4,bl=8,yl=16,_l=4,xl=1,kl=2,Zt=Symbol(),qo="http://www.w3.org/1999/xhtml",wl="http://www.w3.org/2000/svg",Sl="@attach";function $l(){console.warn("https://svelte.dev/e/select_multiple_invalid_value")}function Ml(){console.warn("https://svelte.dev/e/svelte_boundary_reset_noop")}function Bo(e){return e===this.v}function Cl(e,t){return e!=e?t==t:e!==t||e!==null&&typeof e=="object"||typeof e=="function"}function Ko(e){return!Cl(e,this.v)}let Al=!1,yr=null;function nn(e){yr=e}function pe(e,t=!1,r){yr={p:yr,i:!1,c:null,e:null,s:e,x:null,l:null}}function he(e){var t=yr,r=t.e;if(r!==null){t.e=null;for(var n of r)vi(n)}return t.i=!0,yr=t.p,{}}function Wo(){return!0}let Pa=[];function Jo(){var e=Pa;Pa=[],Qi(e)}function Dr(e){if(Pa.length===0&&!Sn){var t=Pa;queueMicrotask(()=>{t===Pa&&Jo()})}Pa.push(e)}function El(){for(;Pa.length>0;)Jo()}function Go(e){var t=nt;if(t===null)return Ge.f|=_a,e;if(!(t.f&Va)&&!(t.f&cn))throw e;ba(e,t)}function ba(e,t){for(;t!==null;){if(t.f&ks){if(!(t.f&Va))throw e;try{t.b.error(e);return}catch(r){e=r}}t=t.parent}throw e}const Pl=-7169;function zt(e,t){e.f=e.f&Pl|t}function Ks(e){e.f&Pr||e.deps===null?zt(e,or):zt(e,jr)}function Qo(e){if(e!==null)for(const t of e)!(t.f&cr)||!(t.f&ja)||(t.f^=ja,Qo(t.deps))}function Yo(e,t,r){e.f&ir?t.add(e):e.f&jr&&r.add(e),Qo(e.deps),zt(e,or)}const Un=new Set;let Te=null,Gn=null,sr=null,hr=[],ss=null,Sn=!1,sn=null,Tl=1;var pa,Ja,Na,Ga,Qa,Ya,ha,Wr,Xa,_r,Ss,$s,Ms,Cs;const oo=class oo{constructor(){Ze(this,_r);Nr(this,"id",Tl++);Nr(this,"current",new Map);Nr(this,"previous",new Map);Ze(this,pa,new Set);Ze(this,Ja,new Set);Ze(this,Na,0);Ze(this,Ga,0);Ze(this,Qa,null);Ze(this,Ya,new Set);Ze(this,ha,new Set);Ze(this,Wr,new Map);Nr(this,"is_fork",!1);Ze(this,Xa,!1)}skip_effect(t){F(this,Wr).has(t)||F(this,Wr).set(t,{d:[],m:[]})}unskip_effect(t){var r=F(this,Wr).get(t);if(r){F(this,Wr).delete(t);for(var n of r.d)zt(n,ir),Xr(n);for(n of r.m)zt(n,jr),Xr(n)}}process(t){var s;hr=[],this.apply();var r=sn=[],n=[];for(const i of t)Qt(this,_r,$s).call(this,i,r,n);if(sn=null,Qt(this,_r,Ss).call(this)){Qt(this,_r,Ms).call(this,n),Qt(this,_r,Ms).call(this,r);for(const[i,l]of F(this,Wr))ti(i,l)}else{Gn=this,Te=null;for(const i of F(this,pa))i(this);F(this,pa).clear(),F(this,Na)===0&&Qt(this,_r,Cs).call(this),uo(n),uo(r),F(this,Ya).clear(),F(this,ha).clear(),Gn=null,(s=F(this,Qa))==null||s.resolve()}sr=null}capture(t,r){r!==Zt&&!this.previous.has(t)&&this.previous.set(t,r),t.f&_a||(this.current.set(t,t.v),sr==null||sr.set(t,t.v))}activate(){Te=this,this.apply()}deactivate(){Te===this&&(Te=null,sr=null)}flush(){var t;if(hr.length>0)Te=this,Xo();else if(F(this,Na)===0&&!this.is_fork){for(const r of F(this,pa))r(this);F(this,pa).clear(),Qt(this,_r,Cs).call(this),(t=F(this,Qa))==null||t.resolve()}this.deactivate()}discard(){for(const t of F(this,Ja))t(this);F(this,Ja).clear()}increment(t){Ue(this,Na,F(this,Na)+1),t&&Ue(this,Ga,F(this,Ga)+1)}decrement(t){Ue(this,Na,F(this,Na)-1),t&&Ue(this,Ga,F(this,Ga)-1),!F(this,Xa)&&(Ue(this,Xa,!0),Dr(()=>{Ue(this,Xa,!1),Qt(this,_r,Ss).call(this)?hr.length>0&&this.flush():this.revive()}))}revive(){for(const t of F(this,Ya))F(this,ha).delete(t),zt(t,ir),Xr(t);for(const t of F(this,ha))zt(t,jr),Xr(t);this.flush()}oncommit(t){F(this,pa).add(t)}ondiscard(t){F(this,Ja).add(t)}settled(){return(F(this,Qa)??Ue(this,Qa,Ho())).promise}static ensure(){if(Te===null){const t=Te=new oo;Un.add(Te),Sn||Dr(()=>{Te===t&&t.flush()})}return Te}apply(){}};pa=new WeakMap,Ja=new WeakMap,Na=new WeakMap,Ga=new WeakMap,Qa=new WeakMap,Ya=new WeakMap,ha=new WeakMap,Wr=new WeakMap,Xa=new WeakMap,_r=new WeakSet,Ss=function(){return this.is_fork||F(this,Ga)>0},$s=function(t,r,n){t.f^=or;for(var s=t.first;s!==null;){var i=s.f,l=(i&(Hr|za))!==0,d=l&&(i&or)!==0,p=(i&gr)!==0,f=d||F(this,Wr).has(s);if(!f&&s.fn!==null){l?p||(s.f^=or):i&cn?r.push(s):i&(an|ns)&&p?n.push(s):In(s)&&(dn(s),i&la&&(F(this,ha).add(s),p&&zt(s,ir)));var y=s.first;if(y!==null){s=y;continue}}for(;s!==null;){var w=s.next;if(w!==null){s=w;break}s=s.parent}}},Ms=function(t){for(var r=0;r<t.length;r+=1)Yo(t[r],F(this,Ya),F(this,ha))},Cs=function(){var i;if(Un.size>1){this.previous.clear();var t=Te,r=sr,n=!0;for(const l of Un){if(l===this){n=!1;continue}const d=[];for(const[f,y]of this.current){if(l.current.has(f))if(n&&y!==l.current.get(f))l.current.set(f,y);else continue;d.push(f)}if(d.length===0)continue;const p=[...l.current.keys()].filter(f=>!this.current.has(f));if(p.length>0){var s=hr;hr=[];const f=new Set,y=new Map;for(const w of d)Zo(w,p,f,y);if(hr.length>0){Te=l,l.apply();for(const w of hr)Qt(i=l,_r,$s).call(i,w,[],[]);l.deactivate()}hr=s}}Te=t,sr=r}F(this,Wr).clear(),Un.delete(this)};let ka=oo;function Fl(e){var t=Sn;Sn=!0;try{for(var r;;){if(El(),hr.length===0&&(Te==null||Te.flush(),hr.length===0))return ss=null,r;Xo()}}finally{Sn=t}}function Xo(){var e=null;try{for(var t=0;hr.length>0;){var r=ka.ensure();if(t++>1e3){var n,s;Nl()}r.process(hr),wa.clear()}}finally{hr=[],ss=null,sn=null}}function Nl(){try{ol()}catch(e){ba(e,ss)}}let Or=null;function uo(e){var t=e.length;if(t!==0){for(var r=0;r<t;){var n=e[r++];if(!(n.f&(ea|gr))&&In(n)&&(Or=new Set,dn(n),n.deps===null&&n.first===null&&n.nodes===null&&n.teardown===null&&n.ac===null&&hi(n),(Or==null?void 0:Or.size)>0)){wa.clear();for(const s of Or){if(s.f&(ea|gr))continue;const i=[s];let l=s.parent;for(;l!==null;)Or.has(l)&&(Or.delete(l),i.push(l)),l=l.parent;for(let d=i.length-1;d>=0;d--){const p=i[d];p.f&(ea|gr)||dn(p)}}Or.clear()}}Or=null}}function Zo(e,t,r,n){if(!r.has(e)&&(r.add(e),e.reactions!==null))for(const s of e.reactions){const i=s.f;i&cr?Zo(s,t,r,n):i&(qs|la)&&!(i&ir)&&ei(s,t,n)&&(zt(s,ir),Xr(s))}}function ei(e,t,r){const n=r.get(e);if(n!==void 0)return n;if(e.deps!==null)for(const s of e.deps){if(rn.call(t,s))return!0;if(s.f&cr&&ei(s,t,r))return r.set(s,!0),!0}return r.set(e,!1),!1}function Xr(e){var t=ss=e,r=t.b;if(r!=null&&r.is_pending&&e.f&(cn|an|ns)&&!(e.f&Va)){r.defer_effect(e);return}for(;t.parent!==null;){t=t.parent;var n=t.f;if(sn!==null&&t===nt&&!(e.f&an))return;if(n&(za|Hr)){if(!(n&or))return;t.f^=or}}hr.push(t)}function ti(e,t){if(!(e.f&Hr&&e.f&or)){e.f&ir?t.d.push(e):e.f&jr&&t.m.push(e),zt(e,or);for(var r=e.first;r!==null;)ti(r,t),r=r.next}}function Ol(e){let t=0,r=Ha(0),n;return()=>{Gs()&&(a(r),ls(()=>(t===0&&(n=Ma(()=>e(()=>$n(r)))),t+=1,()=>{Dr(()=>{t-=1,t===0&&(n==null||n(),n=void 0,$n(r))})})))}}var Ll=ta|un;function Il(e,t,r,n){new Rl(e,t,r,n)}var Er,zs,Jr,Oa,pr,Gr,wr,Lr,na,La,ma,Za,en,tn,sa,ts,Yt,Dl,jl,Hl,As,Kn,Wn,Es;class Rl{constructor(t,r,n,s){Ze(this,Yt);Nr(this,"parent");Nr(this,"is_pending",!1);Nr(this,"transform_error");Ze(this,Er);Ze(this,zs,null);Ze(this,Jr);Ze(this,Oa);Ze(this,pr);Ze(this,Gr,null);Ze(this,wr,null);Ze(this,Lr,null);Ze(this,na,null);Ze(this,La,0);Ze(this,ma,0);Ze(this,Za,!1);Ze(this,en,new Set);Ze(this,tn,new Set);Ze(this,sa,null);Ze(this,ts,Ol(()=>(Ue(this,sa,Ha(F(this,La))),()=>{Ue(this,sa,null)})));var i;Ue(this,Er,t),Ue(this,Jr,r),Ue(this,Oa,l=>{var d=nt;d.b=this,d.f|=ks,n(l)}),this.parent=nt.b,this.transform_error=s??((i=this.parent)==null?void 0:i.transform_error)??(l=>l),Ue(this,pr,vn(()=>{Qt(this,Yt,As).call(this)},Ll))}defer_effect(t){Yo(t,F(this,en),F(this,tn))}is_rendered(){return!this.is_pending&&(!this.parent||this.parent.is_rendered())}has_pending_snippet(){return!!F(this,Jr).pending}update_pending_count(t){Qt(this,Yt,Es).call(this,t),Ue(this,La,F(this,La)+t),!(!F(this,sa)||F(this,Za))&&(Ue(this,Za,!0),Dr(()=>{Ue(this,Za,!1),F(this,sa)&&on(F(this,sa),F(this,La))}))}get_effect_pending(){return F(this,ts).call(this),a(F(this,sa))}error(t){var r=F(this,Jr).onerror;let n=F(this,Jr).failed;if(!r&&!n)throw t;F(this,Gr)&&(dr(F(this,Gr)),Ue(this,Gr,null)),F(this,wr)&&(dr(F(this,wr)),Ue(this,wr,null)),F(this,Lr)&&(dr(F(this,Lr)),Ue(this,Lr,null));var s=!1,i=!1;const l=()=>{if(s){Ml();return}s=!0,i&&ul(),F(this,Lr)!==null&&Ra(F(this,Lr),()=>{Ue(this,Lr,null)}),Qt(this,Yt,Wn).call(this,()=>{ka.ensure(),Qt(this,Yt,As).call(this)})},d=p=>{try{i=!0,r==null||r(p,l),i=!1}catch(f){ba(f,F(this,pr)&&F(this,pr).parent)}n&&Ue(this,Lr,Qt(this,Yt,Wn).call(this,()=>{ka.ensure();try{return br(()=>{var f=nt;f.b=this,f.f|=ks,n(F(this,Er),()=>p,()=>l)})}catch(f){return ba(f,F(this,pr).parent),null}}))};Dr(()=>{var p;try{p=this.transform_error(t)}catch(f){ba(f,F(this,pr)&&F(this,pr).parent);return}p!==null&&typeof p=="object"&&typeof p.then=="function"?p.then(d,f=>ba(f,F(this,pr)&&F(this,pr).parent)):d(p)})}}Er=new WeakMap,zs=new WeakMap,Jr=new WeakMap,Oa=new WeakMap,pr=new WeakMap,Gr=new WeakMap,wr=new WeakMap,Lr=new WeakMap,na=new WeakMap,La=new WeakMap,ma=new WeakMap,Za=new WeakMap,en=new WeakMap,tn=new WeakMap,sa=new WeakMap,ts=new WeakMap,Yt=new WeakSet,Dl=function(){try{Ue(this,Gr,br(()=>F(this,Oa).call(this,F(this,Er))))}catch(t){this.error(t)}},jl=function(t){const r=F(this,Jr).failed;r&&Ue(this,Lr,br(()=>{r(F(this,Er),()=>t,()=>()=>{})}))},Hl=function(){const t=F(this,Jr).pending;t&&(this.is_pending=!0,Ue(this,wr,br(()=>t(F(this,Er)))),Dr(()=>{var r=Ue(this,na,document.createDocumentFragment()),n=ia();r.append(n),Ue(this,Gr,Qt(this,Yt,Wn).call(this,()=>(ka.ensure(),br(()=>F(this,Oa).call(this,n))))),F(this,ma)===0&&(F(this,Er).before(r),Ue(this,na,null),Ra(F(this,wr),()=>{Ue(this,wr,null)}),Qt(this,Yt,Kn).call(this))}))},As=function(){try{if(this.is_pending=this.has_pending_snippet(),Ue(this,ma,0),Ue(this,La,0),Ue(this,Gr,br(()=>{F(this,Oa).call(this,F(this,Er))})),F(this,ma)>0){var t=Ue(this,na,document.createDocumentFragment());Xs(F(this,Gr),t);const r=F(this,Jr).pending;Ue(this,wr,br(()=>r(F(this,Er))))}else Qt(this,Yt,Kn).call(this)}catch(r){this.error(r)}},Kn=function(){this.is_pending=!1;for(const t of F(this,en))zt(t,ir),Xr(t);for(const t of F(this,tn))zt(t,jr),Xr(t);F(this,en).clear(),F(this,tn).clear()},Wn=function(t){var r=nt,n=Ge,s=yr;ra(F(this,pr)),Fr(F(this,pr)),nn(F(this,pr).ctx);try{return t()}catch(i){return Go(i),null}finally{ra(r),Fr(n),nn(s)}},Es=function(t){var r;if(!this.has_pending_snippet()){this.parent&&Qt(r=this.parent,Yt,Es).call(r,t);return}Ue(this,ma,F(this,ma)+t),F(this,ma)===0&&(Qt(this,Yt,Kn).call(this),F(this,wr)&&Ra(F(this,wr),()=>{Ue(this,wr,null)}),F(this,na)&&(F(this,Er).before(F(this,na)),Ue(this,na,null)))};function ri(e,t,r,n){const s=os;var i=e.filter(w=>!w.settled);if(r.length===0&&i.length===0){n(t.map(s));return}var l=nt,d=Ul(),p=i.length===1?i[0].promise:i.length>1?Promise.all(i.map(w=>w.promise)):null;function f(w){d();try{n(w)}catch(x){l.f&ea||ba(x,l)}Ps()}if(r.length===0){p.then(()=>f(t.map(s)));return}function y(){d(),Promise.all(r.map(w=>Vl(w))).then(w=>f([...t.map(s),...w])).catch(w=>ba(w,l))}p?p.then(y):y()}function Ul(){var e=nt,t=Ge,r=yr,n=Te;return function(i=!0){ra(e),Fr(t),nn(r),i&&(n==null||n.activate())}}function Ps(e=!0){ra(null),Fr(null),nn(null),e&&(Te==null||Te.deactivate())}function zl(){var e=nt.b,t=Te,r=e.is_rendered();return e.update_pending_count(1),t.increment(r),()=>{e.update_pending_count(-1),t.decrement(r)}}function os(e){var t=cr|ir,r=Ge!==null&&Ge.f&cr?Ge:null;return nt!==null&&(nt.f|=un),{ctx:yr,deps:null,effects:null,equals:Bo,f:t,fn:e,reactions:null,rv:0,v:Zt,wv:0,parent:r??nt,ac:null}}function Vl(e,t,r){nt===null&&tl();var s=void 0,i=Ha(Zt),l=!Ge,d=new Map;return ad(()=>{var x;var p=Ho();s=p.promise;try{Promise.resolve(e()).then(p.resolve,p.reject).finally(Ps)}catch(O){p.reject(O),Ps()}var f=Te;if(l){var y=zl();(x=d.get(f))==null||x.reject(Ea),d.delete(f),d.set(f,p)}const w=(O,A=void 0)=>{if(f.activate(),A)A!==Ea&&(i.f|=_a,on(i,A));else{i.f&_a&&(i.f^=_a),on(i,O);for(const[N,S]of d){if(d.delete(N),N===f)break;S.reject(Ea)}}y&&y()};p.promise.then(w,O=>w(null,O||"unknown"))}),is(()=>{for(const p of d.values())p.reject(Ea)}),new Promise(p=>{function f(y){function w(){y===s?p(i):f(s)}y.then(w,w)}f(s)})}function et(e){const t=os(e);return yi(t),t}function ai(e){const t=os(e);return t.equals=Ko,t}function ql(e){var t=e.effects;if(t!==null){e.effects=null;for(var r=0;r<t.length;r+=1)dr(t[r])}}function Bl(e){for(var t=e.parent;t!==null;){if(!(t.f&cr))return t.f&ea?null:t;t=t.parent}return null}function Ws(e){var t,r=nt;ra(Bl(e));try{e.f&=~ja,ql(e),t=wi(e)}finally{ra(r)}return t}function ni(e){var t=Ws(e);if(!e.equals(t)&&(e.wv=xi(),(!(Te!=null&&Te.is_fork)||e.deps===null)&&(e.v=t,e.deps===null))){zt(e,or);return}Sa||(sr!==null?(Gs()||Te!=null&&Te.is_fork)&&sr.set(e,t):Ks(e))}function Kl(e){var t,r;if(e.effects!==null)for(const n of e.effects)(n.teardown||n.ac)&&((t=n.teardown)==null||t.call(n),(r=n.ac)==null||r.abort(Ea),n.teardown=Me,n.ac=null,An(n,0),Qs(n))}function si(e){if(e.effects!==null)for(const t of e.effects)t.teardown&&dn(t)}let Ts=new Set;const wa=new Map;let oi=!1;function Ha(e,t){var r={f:0,v:e,reactions:null,equals:Bo,rv:0,wv:0};return r}function L(e,t){const r=Ha(e);return yi(r),r}function Wl(e,t=!1,r=!0){const n=Ha(e);return t||(n.equals=Ko),n}function c(e,t,r=!1){Ge!==null&&(!Rr||Ge.f&co)&&Wo()&&Ge.f&(cr|la|qs|co)&&(Tr===null||!rn.call(Tr,e))&&cl();let n=r?ut(t):t;return on(e,n)}function on(e,t){if(!e.equals(t)){var r=e.v;Sa?wa.set(e,t):wa.set(e,r),e.v=t;var n=ka.ensure();if(n.capture(e,r),e.f&cr){const s=e;e.f&ir&&Ws(s),Ks(s)}e.wv=xi(),ii(e,ir),nt!==null&&nt.f&or&&!(nt.f&(Hr|za))&&(Ar===null?od([e]):Ar.push(e)),!n.is_fork&&Ts.size>0&&!oi&&Jl()}return t}function Jl(){oi=!1;for(const e of Ts)e.f&or&&zt(e,jr),In(e)&&dn(e);Ts.clear()}function $n(e){c(e,e.v+1)}function ii(e,t){var r=e.reactions;if(r!==null)for(var n=r.length,s=0;s<n;s++){var i=r[s],l=i.f,d=(l&ir)===0;if(d&&zt(i,t),l&cr){var p=i;sr==null||sr.delete(p),l&ja||(l&Pr&&(i.f|=ja),ii(p,jr))}else d&&(l&la&&Or!==null&&Or.add(i),Xr(i))}}function ut(e){if(typeof e!="object"||e===null||xa in e)return e;const t=jo(e);if(t!==Ji&&t!==Gi)return e;var r=new Map,n=Vs(e),s=L(0),i=Da,l=d=>{if(Da===i)return d();var p=Ge,f=Da;Fr(null),ho(i);var y=d();return Fr(p),ho(f),y};return n&&r.set("length",L(e.length)),new Proxy(e,{defineProperty(d,p,f){(!("value"in f)||f.configurable===!1||f.enumerable===!1||f.writable===!1)&&ll();var y=r.get(p);return y===void 0?l(()=>{var w=L(f.value);return r.set(p,w),w}):c(y,f.value,!0),!0},deleteProperty(d,p){var f=r.get(p);if(f===void 0){if(p in d){const y=l(()=>L(Zt));r.set(p,y),$n(s)}}else c(f,Zt),$n(s);return!0},get(d,p,f){var O;if(p===xa)return e;var y=r.get(p),w=p in d;if(y===void 0&&(!w||(O=ya(d,p))!=null&&O.writable)&&(y=l(()=>{var A=ut(w?d[p]:Zt),N=L(A);return N}),r.set(p,y)),y!==void 0){var x=a(y);return x===Zt?void 0:x}return Reflect.get(d,p,f)},getOwnPropertyDescriptor(d,p){var f=Reflect.getOwnPropertyDescriptor(d,p);if(f&&"value"in f){var y=r.get(p);y&&(f.value=a(y))}else if(f===void 0){var w=r.get(p),x=w==null?void 0:w.v;if(w!==void 0&&x!==Zt)return{enumerable:!0,configurable:!0,value:x,writable:!0}}return f},has(d,p){var x;if(p===xa)return!0;var f=r.get(p),y=f!==void 0&&f.v!==Zt||Reflect.has(d,p);if(f!==void 0||nt!==null&&(!y||(x=ya(d,p))!=null&&x.writable)){f===void 0&&(f=l(()=>{var O=y?ut(d[p]):Zt,A=L(O);return A}),r.set(p,f));var w=a(f);if(w===Zt)return!1}return y},set(d,p,f,y){var V;var w=r.get(p),x=p in d;if(n&&p==="length")for(var O=f;O<w.v;O+=1){var A=r.get(O+"");A!==void 0?c(A,Zt):O in d&&(A=l(()=>L(Zt)),r.set(O+"",A))}if(w===void 0)(!x||(V=ya(d,p))!=null&&V.writable)&&(w=l(()=>L(void 0)),c(w,ut(f)),r.set(p,w));else{x=w.v!==Zt;var N=l(()=>ut(f));c(w,N)}var S=Reflect.getOwnPropertyDescriptor(d,p);if(S!=null&&S.set&&S.set.call(y,f),!x){if(n&&typeof p=="string"){var $=r.get("length"),J=Number(p);Number.isInteger(J)&&J>=$.v&&c($,J+1)}$n(s)}return!0},ownKeys(d){a(s);var p=Reflect.ownKeys(d).filter(w=>{var x=r.get(w);return x===void 0||x.v!==Zt});for(var[f,y]of r)y.v!==Zt&&!(f in d)&&p.push(f);return p},setPrototypeOf(){dl()}})}function fo(e){try{if(e!==null&&typeof e=="object"&&xa in e)return e[xa]}catch{}return e}function Gl(e,t){return Object.is(fo(e),fo(t))}var vo,li,di,ci;function Ql(){if(vo===void 0){vo=window,li=/Firefox/.test(navigator.userAgent);var e=Element.prototype,t=Node.prototype,r=Text.prototype;di=ya(t,"firstChild").get,ci=ya(t,"nextSibling").get,lo(e)&&(e.__click=void 0,e.__className=void 0,e.__attributes=null,e.__style=void 0,e.__e=void 0),lo(r)&&(r.__t=void 0)}}function ia(e=""){return document.createTextNode(e)}function ln(e){return di.call(e)}function On(e){return ci.call(e)}function o(e,t){return ln(e)}function ge(e,t=!1){{var r=ln(e);return r instanceof Comment&&r.data===""?On(r):r}}function h(e,t=1,r=!1){let n=e;for(;t--;)n=On(n);return n}function Yl(e){e.textContent=""}function ui(){return!1}function fi(e,t,r){return document.createElementNS(t??qo,e,void 0)}function Xl(e,t){if(t){const r=document.body;e.autofocus=!0,Dr(()=>{document.activeElement===r&&e.focus()})}}let go=!1;function Zl(){go||(go=!0,document.addEventListener("reset",e=>{Promise.resolve().then(()=>{var t;if(!e.defaultPrevented)for(const r of e.target.elements)(t=r.__on_r)==null||t.call(r)})},{capture:!0}))}function fn(e){var t=Ge,r=nt;Fr(null),ra(null);try{return e()}finally{Fr(t),ra(r)}}function Js(e,t,r,n=r){e.addEventListener(t,()=>fn(r));const s=e.__on_r;s?e.__on_r=()=>{s(),n(!0)}:e.__on_r=()=>n(!0),Zl()}function ed(e){nt===null&&(Ge===null&&sl(),nl()),Sa&&al()}function td(e,t){var r=t.last;r===null?t.last=t.first=e:(r.next=e,e.prev=r,t.last=e)}function aa(e,t){var r=nt;r!==null&&r.f&gr&&(e|=gr);var n={ctx:yr,deps:null,nodes:null,f:e|ir|Pr,first:null,fn:t,last:null,next:null,parent:r,b:r&&r.b,prev:null,teardown:null,wv:0,ac:null},s=n;if(e&cn)sn!==null?sn.push(n):Xr(n);else if(t!==null){try{dn(n)}catch(l){throw dr(n),l}s.deps===null&&s.teardown===null&&s.nodes===null&&s.first===s.last&&!(s.f&un)&&(s=s.first,e&la&&e&ta&&s!==null&&(s.f|=ta))}if(s!==null&&(s.parent=r,r!==null&&td(s,r),Ge!==null&&Ge.f&cr&&!(e&za))){var i=Ge;(i.effects??(i.effects=[])).push(s)}return n}function Gs(){return Ge!==null&&!Rr}function is(e){const t=aa(an,null);return zt(t,or),t.teardown=e,t}function lr(e){ed();var t=nt.f,r=!Ge&&(t&Hr)!==0&&(t&Va)===0;if(r){var n=yr;(n.e??(n.e=[])).push(e)}else return vi(e)}function vi(e){return aa(cn|Zi,e)}function rd(e){ka.ensure();const t=aa(za|un,e);return(r={})=>new Promise(n=>{r.outro?Ra(t,()=>{dr(t),n(void 0)}):(dr(t),n(void 0))})}function Ln(e){return aa(cn,e)}function ad(e){return aa(qs|un,e)}function ls(e,t=0){return aa(an|t,e)}function C(e,t=[],r=[],n=[]){ri(n,t,r,s=>{aa(an,()=>e(...s.map(a)))})}function vn(e,t=0){var r=aa(la|t,e);return r}function gi(e,t=0){var r=aa(ns|t,e);return r}function br(e){return aa(Hr|un,e)}function pi(e){var t=e.teardown;if(t!==null){const r=Sa,n=Ge;po(!0),Fr(null);try{t.call(null)}finally{po(r),Fr(n)}}}function Qs(e,t=!1){var r=e.first;for(e.first=e.last=null;r!==null;){const s=r.ac;s!==null&&fn(()=>{s.abort(Ea)});var n=r.next;r.f&za?r.parent=null:dr(r,t),r=n}}function nd(e){for(var t=e.first;t!==null;){var r=t.next;t.f&Hr||dr(t),t=r}}function dr(e,t=!0){var r=!1;(t||e.f&Xi)&&e.nodes!==null&&e.nodes.end!==null&&(sd(e.nodes.start,e.nodes.end),r=!0),Qs(e,t&&!r),An(e,0),zt(e,ea);var n=e.nodes&&e.nodes.t;if(n!==null)for(const i of n)i.stop();pi(e);var s=e.parent;s!==null&&s.first!==null&&hi(e),e.next=e.prev=e.teardown=e.ctx=e.deps=e.fn=e.nodes=e.ac=null}function sd(e,t){for(;e!==null;){var r=e===t?null:On(e);e.remove(),e=r}}function hi(e){var t=e.parent,r=e.prev,n=e.next;r!==null&&(r.next=n),n!==null&&(n.prev=r),t!==null&&(t.first===e&&(t.first=n),t.last===e&&(t.last=r))}function Ra(e,t,r=!0){var n=[];mi(e,n,!0);var s=()=>{r&&dr(e),t&&t()},i=n.length;if(i>0){var l=()=>--i||s();for(var d of n)d.out(l)}else s()}function mi(e,t,r){if(!(e.f&gr)){e.f^=gr;var n=e.nodes&&e.nodes.t;if(n!==null)for(const d of n)(d.is_global||r)&&t.push(d);for(var s=e.first;s!==null;){var i=s.next,l=(s.f&ta)!==0||(s.f&Hr)!==0&&(e.f&la)!==0;mi(s,t,l?r:!1),s=i}}}function Ys(e){bi(e,!0)}function bi(e,t){if(e.f&gr){e.f^=gr;for(var r=e.first;r!==null;){var n=r.next,s=(r.f&ta)!==0||(r.f&Hr)!==0;bi(r,s?t:!1),r=n}var i=e.nodes&&e.nodes.t;if(i!==null)for(const l of i)(l.is_global||t)&&l.in()}}function Xs(e,t){if(e.nodes)for(var r=e.nodes.start,n=e.nodes.end;r!==null;){var s=r===n?null:On(r);t.append(r),r=s}}let Jn=!1,Sa=!1;function po(e){Sa=e}let Ge=null,Rr=!1;function Fr(e){Ge=e}let nt=null;function ra(e){nt=e}let Tr=null;function yi(e){Ge!==null&&(Tr===null?Tr=[e]:Tr.push(e))}let mr=null,kr=0,Ar=null;function od(e){Ar=e}let _i=1,Ta=0,Da=Ta;function ho(e){Da=e}function xi(){return++_i}function In(e){var t=e.f;if(t&ir)return!0;if(t&cr&&(e.f&=~ja),t&jr){for(var r=e.deps,n=r.length,s=0;s<n;s++){var i=r[s];if(In(i)&&ni(i),i.wv>e.wv)return!0}t&Pr&&sr===null&&zt(e,or)}return!1}function ki(e,t,r=!0){var n=e.reactions;if(n!==null&&!(Tr!==null&&rn.call(Tr,e)))for(var s=0;s<n.length;s++){var i=n[s];i.f&cr?ki(i,t,!1):t===i&&(r?zt(i,ir):i.f&or&&zt(i,jr),Xr(i))}}function wi(e){var N;var t=mr,r=kr,n=Ar,s=Ge,i=Tr,l=yr,d=Rr,p=Da,f=e.f;mr=null,kr=0,Ar=null,Ge=f&(Hr|za)?null:e,Tr=null,nn(e.ctx),Rr=!1,Da=++Ta,e.ac!==null&&(fn(()=>{e.ac.abort(Ea)}),e.ac=null);try{e.f|=ws;var y=e.fn,w=y();e.f|=Va;var x=e.deps,O=Te==null?void 0:Te.is_fork;if(mr!==null){var A;if(O||An(e,kr),x!==null&&kr>0)for(x.length=kr+mr.length,A=0;A<mr.length;A++)x[kr+A]=mr[A];else e.deps=x=mr;if(Gs()&&e.f&Pr)for(A=kr;A<x.length;A++)((N=x[A]).reactions??(N.reactions=[])).push(e)}else!O&&x!==null&&kr<x.length&&(An(e,kr),x.length=kr);if(Wo()&&Ar!==null&&!Rr&&x!==null&&!(e.f&(cr|jr|ir)))for(A=0;A<Ar.length;A++)ki(Ar[A],e);if(s!==null&&s!==e){if(Ta++,s.deps!==null)for(let S=0;S<r;S+=1)s.deps[S].rv=Ta;if(t!==null)for(const S of t)S.rv=Ta;Ar!==null&&(n===null?n=Ar:n.push(...Ar))}return e.f&_a&&(e.f^=_a),w}catch(S){return Go(S)}finally{e.f^=ws,mr=t,kr=r,Ar=n,Ge=s,Tr=i,nn(l),Rr=d,Da=p}}function id(e,t){let r=t.reactions;if(r!==null){var n=Bi.call(r,e);if(n!==-1){var s=r.length-1;s===0?r=t.reactions=null:(r[n]=r[s],r.pop())}}if(r===null&&t.f&cr&&(mr===null||!rn.call(mr,t))){var i=t;i.f&Pr&&(i.f^=Pr,i.f&=~ja),Ks(i),Kl(i),An(i,0)}}function An(e,t){var r=e.deps;if(r!==null)for(var n=t;n<r.length;n++)id(e,r[n])}function dn(e){var t=e.f;if(!(t&ea)){zt(e,or);var r=nt,n=Jn;nt=e,Jn=!0;try{t&(la|ns)?nd(e):Qs(e),pi(e);var s=wi(e);e.teardown=typeof s=="function"?s:null,e.wv=_i;var i;xs&&Al&&e.f&ir&&e.deps}finally{Jn=n,nt=r}}}async function Fs(){await Promise.resolve(),Fl()}function a(e){var t=e.f,r=(t&cr)!==0;if(Ge!==null&&!Rr){var n=nt!==null&&(nt.f&ea)!==0;if(!n&&(Tr===null||!rn.call(Tr,e))){var s=Ge.deps;if(Ge.f&ws)e.rv<Ta&&(e.rv=Ta,mr===null&&s!==null&&s[kr]===e?kr++:mr===null?mr=[e]:mr.push(e));else{(Ge.deps??(Ge.deps=[])).push(e);var i=e.reactions;i===null?e.reactions=[Ge]:rn.call(i,Ge)||i.push(Ge)}}}if(Sa&&wa.has(e))return wa.get(e);if(r){var l=e;if(Sa){var d=l.v;return(!(l.f&or)&&l.reactions!==null||$i(l))&&(d=Ws(l)),wa.set(l,d),d}var p=(l.f&Pr)===0&&!Rr&&Ge!==null&&(Jn||(Ge.f&Pr)!==0),f=(l.f&Va)===0;In(l)&&(p&&(l.f|=Pr),ni(l)),p&&!f&&(si(l),Si(l))}if(sr!=null&&sr.has(e))return sr.get(e);if(e.f&_a)throw e.v;return e.v}function Si(e){if(e.f|=Pr,e.deps!==null)for(const t of e.deps)(t.reactions??(t.reactions=[])).push(e),t.f&cr&&!(t.f&Pr)&&(si(t),Si(t))}function $i(e){if(e.v===Zt)return!0;if(e.deps===null)return!1;for(const t of e.deps)if(wa.has(t)||t.f&cr&&$i(t))return!0;return!1}function Ma(e){var t=Rr;try{return Rr=!0,e()}finally{Rr=t}}function ld(e){return e.endsWith("capture")&&e!=="gotpointercapture"&&e!=="lostpointercapture"}const dd=["beforeinput","click","change","dblclick","contextmenu","focusin","focusout","input","keydown","keyup","mousedown","mousemove","mouseout","mouseover","mouseup","pointerdown","pointermove","pointerout","pointerover","pointerup","touchend","touchmove","touchstart"];function cd(e){return dd.includes(e)}const ud={formnovalidate:"formNoValidate",ismap:"isMap",nomodule:"noModule",playsinline:"playsInline",readonly:"readOnly",defaultvalue:"defaultValue",defaultchecked:"defaultChecked",srcobject:"srcObject",novalidate:"noValidate",allowfullscreen:"allowFullscreen",disablepictureinpicture:"disablePictureInPicture",disableremoteplayback:"disableRemotePlayback"};function fd(e){return e=e.toLowerCase(),ud[e]??e}const vd=["touchstart","touchmove"];function gd(e){return vd.includes(e)}const Fa=Symbol("events"),Mi=new Set,Ns=new Set;function Ci(e,t,r,n={}){function s(i){if(n.capture||Os.call(t,i),!i.cancelBubble)return fn(()=>r==null?void 0:r.call(this,i))}return e.startsWith("pointer")||e.startsWith("touch")||e==="wheel"?Dr(()=>{t.addEventListener(e,s,n)}):t.addEventListener(e,s,n),s}function va(e,t,r,n,s){var i={capture:n,passive:s},l=Ci(e,t,r,i);(t===document.body||t===window||t===document||t instanceof HTMLMediaElement)&&is(()=>{t.removeEventListener(e,l,i)})}function re(e,t,r){(t[Fa]??(t[Fa]={}))[e]=r}function Ur(e){for(var t=0;t<e.length;t++)Mi.add(e[t]);for(var r of Ns)r(e)}let mo=null;function Os(e){var S,$;var t=this,r=t.ownerDocument,n=e.type,s=((S=e.composedPath)==null?void 0:S.call(e))||[],i=s[0]||e.target;mo=e;var l=0,d=mo===e&&e[Fa];if(d){var p=s.indexOf(d);if(p!==-1&&(t===document||t===window)){e[Fa]=t;return}var f=s.indexOf(t);if(f===-1)return;p<=f&&(l=p)}if(i=s[l]||e.target,i!==t){Ki(e,"currentTarget",{configurable:!0,get(){return i||r}});var y=Ge,w=nt;Fr(null),ra(null);try{for(var x,O=[];i!==null;){var A=i.assignedSlot||i.parentNode||i.host||null;try{var N=($=i[Fa])==null?void 0:$[n];N!=null&&(!i.disabled||e.target===i)&&N.call(i,e)}catch(J){x?O.push(J):x=J}if(e.cancelBubble||A===t||A===null)break;i=A}if(x){for(let J of O)queueMicrotask(()=>{throw J});throw x}}finally{e[Fa]=t,delete e.currentTarget,Fr(y),ra(w)}}}var Ro;const vs=((Ro=globalThis==null?void 0:globalThis.window)==null?void 0:Ro.trustedTypes)&&globalThis.window.trustedTypes.createPolicy("svelte-trusted-html",{createHTML:e=>e});function pd(e){return(vs==null?void 0:vs.createHTML(e))??e}function Ai(e){var t=fi("template");return t.innerHTML=pd(e.replaceAll("<!>","<!---->")),t.content}function En(e,t){var r=nt;r.nodes===null&&(r.nodes={start:e,end:t,a:null,t:null})}function k(e,t){var r=(t&xl)!==0,n=(t&kl)!==0,s,i=!e.startsWith("<!>");return()=>{s===void 0&&(s=Ai(i?e:"<!>"+e),r||(s=ln(s)));var l=n||li?document.importNode(s,!0):s.cloneNode(!0);if(r){var d=ln(l),p=l.lastChild;En(d,p)}else En(l,l);return l}}function hd(e,t,r="svg"){var n=!e.startsWith("<!>"),s=`<${r}>${n?e:"<!>"+e}</${r}>`,i;return()=>{if(!i){var l=Ai(s),d=ln(l);i=ln(d)}var p=i.cloneNode(!0);return En(p,p),p}}function md(e,t){return hd(e,t,"svg")}function Ie(){var e=document.createDocumentFragment(),t=document.createComment(""),r=ia();return e.append(t,r),En(t,r),e}function m(e,t){e!==null&&e.before(t)}let Qn=!0;function zn(e){Qn=e}function v(e,t){var r=t==null?"":typeof t=="object"?`${t}`:t;r!==(e.__t??(e.__t=e.nodeValue))&&(e.__t=r,e.nodeValue=`${r}`)}function bd(e,t){return yd(e,t)}const Vn=new Map;function yd(e,{target:t,anchor:r,props:n={},events:s,context:i,intro:l=!0,transformError:d}){Ql();var p=void 0,f=rd(()=>{var y=r??t.appendChild(ia());Il(y,{pending:()=>{}},O=>{pe({});var A=yr;i&&(A.c=i),s&&(n.$$events=s),Qn=l,p=e(O,n)||{},Qn=!0,he()},d);var w=new Set,x=O=>{for(var A=0;A<O.length;A++){var N=O[A];if(!w.has(N)){w.add(N);var S=gd(N);for(const V of[t,document]){var $=Vn.get(V);$===void 0&&($=new Map,Vn.set(V,$));var J=$.get(N);J===void 0?(V.addEventListener(N,Os,{passive:S}),$.set(N,1)):$.set(N,J+1)}}}};return x(as(Mi)),Ns.add(x),()=>{var S;for(var O of w)for(const $ of[t,document]){var A=Vn.get($),N=A.get(O);--N==0?($.removeEventListener(O,Os),A.delete(O),A.size===0&&Vn.delete($)):A.set(O,N)}Ns.delete(x),y!==r&&((S=y.parentNode)==null||S.removeChild(y))}});return _d.set(p,f),p}let _d=new WeakMap;var Ir,Qr,Sr,Ia,Fn,Nn,rs;class ds{constructor(t,r=!0){Nr(this,"anchor");Ze(this,Ir,new Map);Ze(this,Qr,new Map);Ze(this,Sr,new Map);Ze(this,Ia,new Set);Ze(this,Fn,!0);Ze(this,Nn,t=>{if(F(this,Ir).has(t)){var r=F(this,Ir).get(t),n=F(this,Qr).get(r);if(n)Ys(n),F(this,Ia).delete(r);else{var s=F(this,Sr).get(r);s&&!(s.effect.f&gr)&&(F(this,Qr).set(r,s.effect),F(this,Sr).delete(r),s.fragment.lastChild.remove(),this.anchor.before(s.fragment),n=s.effect)}for(const[i,l]of F(this,Ir)){if(F(this,Ir).delete(i),i===t)break;const d=F(this,Sr).get(l);d&&(dr(d.effect),F(this,Sr).delete(l))}for(const[i,l]of F(this,Qr)){if(i===r||F(this,Ia).has(i)||l.f&gr)continue;const d=()=>{if(Array.from(F(this,Ir).values()).includes(i)){var f=document.createDocumentFragment();Xs(l,f),f.append(ia()),F(this,Sr).set(i,{effect:l,fragment:f})}else dr(l);F(this,Ia).delete(i),F(this,Qr).delete(i)};F(this,Fn)||!n?(F(this,Ia).add(i),Ra(l,d,!1)):d()}}});Ze(this,rs,t=>{F(this,Ir).delete(t);const r=Array.from(F(this,Ir).values());for(const[n,s]of F(this,Sr))r.includes(n)||(dr(s.effect),F(this,Sr).delete(n))});this.anchor=t,Ue(this,Fn,r)}ensure(t,r){var n=Te,s=ui();if(r&&!F(this,Qr).has(t)&&!F(this,Sr).has(t))if(s){var i=document.createDocumentFragment(),l=ia();i.append(l),F(this,Sr).set(t,{effect:br(()=>r(l)),fragment:i})}else F(this,Qr).set(t,br(()=>r(this.anchor)));if(F(this,Ir).set(n,t),s){for(const[d,p]of F(this,Qr))d===t?n.unskip_effect(p):n.skip_effect(p);for(const[d,p]of F(this,Sr))d===t?n.unskip_effect(p.effect):n.skip_effect(p.effect);n.oncommit(F(this,Nn)),n.ondiscard(F(this,rs))}else F(this,Nn).call(this,n)}}Ir=new WeakMap,Qr=new WeakMap,Sr=new WeakMap,Ia=new WeakMap,Fn=new WeakMap,Nn=new WeakMap,rs=new WeakMap;function W(e,t,r=!1){var n=new ds(e),s=r?ta:0;function i(l,d){n.ensure(l,d)}vn(()=>{var l=!1;t((d,p=0)=>{l=!0,i(p,d)}),l||i(-1,null)},s)}function It(e,t){return t}function xd(e,t,r){for(var n=[],s=t.length,i,l=t.length,d=0;d<s;d++){let w=t[d];Ra(w,()=>{if(i){if(i.pending.delete(w),i.done.add(w),i.pending.size===0){var x=e.outrogroups;Ls(e,as(i.done)),x.delete(i),x.size===0&&(e.outrogroups=null)}}else l-=1},!1)}if(l===0){var p=n.length===0&&r!==null;if(p){var f=r,y=f.parentNode;Yl(y),y.append(f),e.items.clear()}Ls(e,t,!p)}else i={pending:new Set(t),done:new Set},(e.outrogroups??(e.outrogroups=new Set)).add(i)}function Ls(e,t,r=!0){var n;if(e.pending.size>0){n=new Set;for(const l of e.pending.values())for(const d of l)n.add(e.items.get(d).e)}for(var s=0;s<t.length;s++){var i=t[s];if(n!=null&&n.has(i)){i.f|=Yr;const l=document.createDocumentFragment();Xs(i,l)}else dr(t[s],r)}}var bo;function at(e,t,r,n,s,i=null){var l=e,d=new Map,p=(t&Vo)!==0;if(p){var f=e;l=f.appendChild(ia())}var y=null,w=ai(()=>{var V=r();return Vs(V)?V:V==null?[]:as(V)}),x,O=new Map,A=!0;function N(V){J.effect.f&ea||(J.pending.delete(V),J.fallback=y,kd(J,x,l,t,n),y!==null&&(x.length===0?y.f&Yr?(y.f^=Yr,wn(y,null,l)):Ys(y):Ra(y,()=>{y=null})))}function S(V){J.pending.delete(V)}var $=vn(()=>{x=a(w);for(var V=x.length,E=new Set,M=Te,R=ui(),D=0;D<V;D+=1){var B=x[D],X=n(B,D),oe=A?null:d.get(X);oe?(oe.v&&on(oe.v,B),oe.i&&on(oe.i,D),R&&M.unskip_effect(oe.e)):(oe=wd(d,A?l:bo??(bo=ia()),B,X,D,s,t,r),A||(oe.e.f|=Yr),d.set(X,oe)),E.add(X)}if(V===0&&i&&!y&&(A?y=br(()=>i(l)):(y=br(()=>i(bo??(bo=ia()))),y.f|=Yr)),V>E.size&&rl(),!A)if(O.set(M,E),R){for(const[ye,_e]of d)E.has(ye)||M.skip_effect(_e.e);M.oncommit(N),M.ondiscard(S)}else N(M);a(w)}),J={effect:$,items:d,pending:O,outrogroups:null,fallback:y};A=!1}function bn(e){for(;e!==null&&!(e.f&Hr);)e=e.next;return e}function kd(e,t,r,n,s){var oe,ye,_e,G,ne,ce,de,ze,Ce;var i=(n&gl)!==0,l=t.length,d=e.items,p=bn(e.effect.first),f,y=null,w,x=[],O=[],A,N,S,$;if(i)for($=0;$<l;$+=1)A=t[$],N=s(A,$),S=d.get(N).e,S.f&Yr||((ye=(oe=S.nodes)==null?void 0:oe.a)==null||ye.measure(),(w??(w=new Set)).add(S));for($=0;$<l;$+=1){if(A=t[$],N=s(A,$),S=d.get(N).e,e.outrogroups!==null)for(const Y of e.outrogroups)Y.pending.delete(S),Y.done.delete(S);if(S.f&Yr)if(S.f^=Yr,S===p)wn(S,null,r);else{var J=y?y.next:p;S===e.effect.last&&(e.effect.last=S.prev),S.prev&&(S.prev.next=S.next),S.next&&(S.next.prev=S.prev),fa(e,y,S),fa(e,S,J),wn(S,J,r),y=S,x=[],O=[],p=bn(y.next);continue}if(S.f&gr&&(Ys(S),i&&((G=(_e=S.nodes)==null?void 0:_e.a)==null||G.unfix(),(w??(w=new Set)).delete(S))),S!==p){if(f!==void 0&&f.has(S)){if(x.length<O.length){var V=O[0],E;y=V.prev;var M=x[0],R=x[x.length-1];for(E=0;E<x.length;E+=1)wn(x[E],V,r);for(E=0;E<O.length;E+=1)f.delete(O[E]);fa(e,M.prev,R.next),fa(e,y,M),fa(e,R,V),p=V,y=R,$-=1,x=[],O=[]}else f.delete(S),wn(S,p,r),fa(e,S.prev,S.next),fa(e,S,y===null?e.effect.first:y.next),fa(e,y,S),y=S;continue}for(x=[],O=[];p!==null&&p!==S;)(f??(f=new Set)).add(p),O.push(p),p=bn(p.next);if(p===null)continue}S.f&Yr||x.push(S),y=S,p=bn(S.next)}if(e.outrogroups!==null){for(const Y of e.outrogroups)Y.pending.size===0&&(Ls(e,as(Y.done)),(ne=e.outrogroups)==null||ne.delete(Y));e.outrogroups.size===0&&(e.outrogroups=null)}if(p!==null||f!==void 0){var D=[];if(f!==void 0)for(S of f)S.f&gr||D.push(S);for(;p!==null;)!(p.f&gr)&&p!==e.fallback&&D.push(p),p=bn(p.next);var B=D.length;if(B>0){var X=n&Vo&&l===0?r:null;if(i){for($=0;$<B;$+=1)(de=(ce=D[$].nodes)==null?void 0:ce.a)==null||de.measure();for($=0;$<B;$+=1)(Ce=(ze=D[$].nodes)==null?void 0:ze.a)==null||Ce.fix()}xd(e,D,X)}}i&&Dr(()=>{var Y,Qe;if(w!==void 0)for(S of w)(Qe=(Y=S.nodes)==null?void 0:Y.a)==null||Qe.apply()})}function wd(e,t,r,n,s,i,l,d){var p=l&fl?l&pl?Ha(r):Wl(r,!1,!1):null,f=l&vl?Ha(s):null;return{v:p,i:f,e:br(()=>(i(t,p??r,f??s,d),()=>{e.delete(n)}))}}function wn(e,t,r){if(e.nodes)for(var n=e.nodes.start,s=e.nodes.end,i=t&&!(t.f&Yr)?t.nodes.start:r;n!==null;){var l=On(n);if(i.before(n),n===s)return;n=l}}function fa(e,t,r){t===null?e.effect.first=r:t.next=r,r===null?e.effect.last=t:r.prev=t}function st(e,t,...r){var n=new ds(e);vn(()=>{const s=t()??null;n.ensure(s,s&&(i=>s(i,...r)))},ta)}function Sd(e,t,r){var n=new ds(e);vn(()=>{var s=t()??null;n.ensure(s,s&&(i=>r(i,s)))},ta)}const $d=()=>performance.now(),oa={tick:e=>requestAnimationFrame(e),now:()=>$d(),tasks:new Set};function Ei(){const e=oa.now();oa.tasks.forEach(t=>{t.c(e)||(oa.tasks.delete(t),t.f())}),oa.tasks.size!==0&&oa.tick(Ei)}function Md(e){let t;return oa.tasks.size===0&&oa.tick(Ei),{promise:new Promise(r=>{oa.tasks.add(t={c:e,f:r})}),abort(){oa.tasks.delete(t)}}}function Yn(e,t){fn(()=>{e.dispatchEvent(new CustomEvent(t))})}function Cd(e){if(e==="float")return"cssFloat";if(e==="offset")return"cssOffset";if(e.startsWith("--"))return e;const t=e.split("-");return t.length===1?t[0]:t[0]+t.slice(1).map(r=>r[0].toUpperCase()+r.slice(1)).join("")}function yo(e){const t={},r=e.split(";");for(const n of r){const[s,i]=n.split(":");if(!s||i===void 0)break;const l=Cd(s.trim());t[l]=i.trim()}return t}const Ad=e=>e;function qn(e,t,r,n){var S;var s=(e&_l)!==0,i="both",l,d=t.inert,p=t.style.overflow,f,y;function w(){return fn(()=>l??(l=r()(t,(n==null?void 0:n())??{},{direction:i})))}var x={is_global:s,in(){t.inert=d,f=Is(t,w(),y,1,()=>{Yn(t,"introend"),f==null||f.abort(),f=l=void 0,t.style.overflow=p})},out($){t.inert=!0,y=Is(t,w(),f,0,()=>{Yn(t,"outroend"),$==null||$()})},stop:()=>{f==null||f.abort(),y==null||y.abort()}},O=nt;if(((S=O.nodes).t??(S.t=[])).push(x),Qn){var A=s;if(!A){for(var N=O.parent;N&&N.f&ta;)for(;(N=N.parent)&&!(N.f&la););A=!N||(N.f&Va)!==0}A&&Ln(()=>{Ma(()=>x.in())})}}function Is(e,t,r,n,s){var i=n===1;if(Ka(t)){var l,d=!1;return Dr(()=>{if(!d){var S=t({direction:i?"in":"out"});l=Is(e,S,r,n,s)}}),{abort:()=>{d=!0,l==null||l.abort()},deactivate:()=>l.deactivate(),reset:()=>l.reset(),t:()=>l.t()}}if(r==null||r.deactivate(),!(t!=null&&t.duration)&&!(t!=null&&t.delay))return Yn(e,i?"introstart":"outrostart"),s(),{abort:Me,deactivate:Me,reset:Me,t:()=>n};const{delay:p=0,css:f,tick:y,easing:w=Ad}=t;var x=[];if(i&&r===void 0&&(y&&y(0,1),f)){var O=yo(f(0,1));x.push(O,O)}var A=()=>1-n,N=e.animate(x,{duration:p,fill:"forwards"});return N.onfinish=()=>{N.cancel(),Yn(e,i?"introstart":"outrostart");var S=(r==null?void 0:r.t())??1-n;r==null||r.abort();var $=n-S,J=t.duration*Math.abs($),V=[];if(J>0){var E=!1;if(f)for(var M=Math.ceil(J/16.666666666666668),R=0;R<=M;R+=1){var D=S+$*w(R/M),B=yo(f(D,1-D));V.push(B),E||(E=B.overflow==="hidden")}E&&(e.style.overflow="hidden"),A=()=>{var X=N.currentTime;return S+$*w(X/J)},y&&Md(()=>{if(N.playState!=="running")return!1;var X=A();return y(X,1-X),!0})}N=e.animate(V,{duration:J,fill:"forwards"}),N.onfinish=()=>{A=()=>n,y==null||y(n,1-n),s()}},{abort:()=>{N&&(N.cancel(),N.effect=null,N.onfinish=Me)},deactivate:()=>{s=Me},reset:()=>{n===0&&(y==null||y(1,0))},t:()=>A()}}function Ed(e,t,r,n,s,i){var l=null,d=e,p=new ds(d,!1);vn(()=>{const f=t()||null;var y=wl;if(f===null){p.ensure(null,null),zn(!0);return}return p.ensure(f,w=>{if(f){if(l=fi(f,y),En(l,l),n){var x=l.appendChild(ia());n(l,x)}nt.nodes.end=l,w.before(l)}}),zn(!0),()=>{f&&zn(!1)}},ta),is(()=>{zn(!0)})}function Pd(e,t){var r=void 0,n;gi(()=>{r!==(r=t())&&(n&&(dr(n),n=null),r&&(n=br(()=>{Ln(()=>r(e))})))})}function Pi(e){var t,r,n="";if(typeof e=="string"||typeof e=="number")n+=e;else if(typeof e=="object")if(Array.isArray(e)){var s=e.length;for(t=0;t<s;t++)e[t]&&(r=Pi(e[t]))&&(n&&(n+=" "),n+=r)}else for(r in e)e[r]&&(n&&(n+=" "),n+=r);return n}function Td(){for(var e,t,r=0,n="",s=arguments.length;r<s;r++)(e=arguments[r])&&(t=Pi(e))&&(n&&(n+=" "),n+=t);return n}function Zs(e){return typeof e=="object"?Td(e):e??""}const _o=[...` 	
\r\f \v\uFEFF`];function Fd(e,t,r){var n=e==null?"":""+e;if(t&&(n=n?n+" "+t:t),r){for(var s of Object.keys(r))if(r[s])n=n?n+" "+s:s;else if(n.length)for(var i=s.length,l=0;(l=n.indexOf(s,l))>=0;){var d=l+i;(l===0||_o.includes(n[l-1]))&&(d===n.length||_o.includes(n[d]))?n=(l===0?"":n.substring(0,l))+n.substring(d+1):l=d}}return n===""?null:n}function xo(e,t=!1){var r=t?" !important;":";",n="";for(var s of Object.keys(e)){var i=e[s];i!=null&&i!==""&&(n+=" "+s+": "+i+r)}return n}function gs(e){return e[0]!=="-"||e[1]!=="-"?e.toLowerCase():e}function Nd(e,t){if(t){var r="",n,s;if(Array.isArray(t)?(n=t[0],s=t[1]):n=t,e){e=String(e).replaceAll(/\s*\/\*.*?\*\/\s*/g,"").trim();var i=!1,l=0,d=!1,p=[];n&&p.push(...Object.keys(n).map(gs)),s&&p.push(...Object.keys(s).map(gs));var f=0,y=-1;const N=e.length;for(var w=0;w<N;w++){var x=e[w];if(d?x==="/"&&e[w-1]==="*"&&(d=!1):i?i===x&&(i=!1):x==="/"&&e[w+1]==="*"?d=!0:x==='"'||x==="'"?i=x:x==="("?l++:x===")"&&l--,!d&&i===!1&&l===0){if(x===":"&&y===-1)y=w;else if(x===";"||w===N-1){if(y!==-1){var O=gs(e.substring(f,y).trim());if(!p.includes(O)){x!==";"&&w++;var A=e.substring(f,w).trim();r+=" "+A+";"}}f=w+1,y=-1}}}}return n&&(r+=xo(n)),s&&(r+=xo(s,!0)),r=r.trim(),r===""?null:r}return e==null?null:String(e)}function _t(e,t,r,n,s,i){var l=e.__className;if(l!==r||l===void 0){var d=Fd(r,n,i);d==null?e.removeAttribute("class"):t?e.className=d:e.setAttribute("class",d),e.__className=r}else if(i&&s!==i)for(var p in i){var f=!!i[p];(s==null||f!==!!s[p])&&e.classList.toggle(p,f)}return i}function ps(e,t={},r,n){for(var s in r){var i=r[s];t[s]!==i&&(r[s]==null?e.style.removeProperty(s):e.style.setProperty(s,i,n))}}function Od(e,t,r,n){var s=e.__style;if(s!==t){var i=Nd(t,n);i==null?e.removeAttribute("style"):e.style.cssText=i,e.__style=t}else n&&(Array.isArray(n)?(ps(e,r==null?void 0:r[0],n[0]),ps(e,r==null?void 0:r[1],n[1],"important")):ps(e,r,n));return n}function Pn(e,t,r=!1){if(e.multiple){if(t==null)return;if(!Vs(t))return $l();for(var n of e.options)n.selected=t.includes(Mn(n));return}for(n of e.options){var s=Mn(n);if(Gl(s,t)){n.selected=!0;return}}(!r||t!==void 0)&&(e.selectedIndex=-1)}function eo(e){var t=new MutationObserver(()=>{Pn(e,e.__value)});t.observe(e,{childList:!0,subtree:!0,attributes:!0,attributeFilter:["value"]}),is(()=>{t.disconnect()})}function Ua(e,t,r=t){var n=new WeakSet,s=!0;Js(e,"change",i=>{var l=i?"[selected]":":checked",d;if(e.multiple)d=[].map.call(e.querySelectorAll(l),Mn);else{var p=e.querySelector(l)??e.querySelector("option:not([disabled])");d=p&&Mn(p)}r(d),Te!==null&&n.add(Te)}),Ln(()=>{var i=t();if(e===document.activeElement){var l=Gn??Te;if(n.has(l))return}if(Pn(e,i,s),s&&i===void 0){var d=e.querySelector(":checked");d!==null&&(i=Mn(d),r(i))}e.__value=i,s=!1}),eo(e)}function Mn(e){return"__value"in e?e.__value:e.value}const yn=Symbol("class"),_n=Symbol("style"),Ti=Symbol("is custom element"),Fi=Symbol("is html"),Ld=Bs?"option":"OPTION",Id=Bs?"select":"SELECT",Rd=Bs?"progress":"PROGRESS";function xn(e,t){var r=to(e);r.value===(r.value=t??void 0)||e.value===t&&(t!==0||e.nodeName!==Rd)||(e.value=t??"")}function Dd(e,t){t?e.hasAttribute("selected")||e.setAttribute("selected",""):e.removeAttribute("selected")}function ve(e,t,r,n){var s=to(e);s[t]!==(s[t]=r)&&(t==="loading"&&(e[el]=r),r==null?e.removeAttribute(t):typeof r!="string"&&Ni(e).includes(t)?e[t]=r:e.setAttribute(t,r))}function jd(e,t,r,n,s=!1,i=!1){var l=to(e),d=l[Ti],p=!l[Fi],f=t||{},y=e.nodeName===Ld;for(var w in t)w in r||(r[w]=null);r.class?r.class=Zs(r.class):r[yn]&&(r.class=null),r[_n]&&(r.style??(r.style=null));var x=Ni(e);for(const E in r){let M=r[E];if(y&&E==="value"&&M==null){e.value=e.__value="",f[E]=M;continue}if(E==="class"){var O=e.namespaceURI==="http://www.w3.org/1999/xhtml";_t(e,O,M,n,t==null?void 0:t[yn],r[yn]),f[E]=M,f[yn]=r[yn];continue}if(E==="style"){Od(e,M,t==null?void 0:t[_n],r[_n]),f[E]=M,f[_n]=r[_n];continue}var A=f[E];if(!(M===A&&!(M===void 0&&e.hasAttribute(E)))){f[E]=M;var N=E[0]+E[1];if(N!=="$$")if(N==="on"){const R={},D="$$"+E;let B=E.slice(2);var S=cd(B);if(ld(B)&&(B=B.slice(0,-7),R.capture=!0),!S&&A){if(M!=null)continue;e.removeEventListener(B,f[D],R),f[D]=null}if(S)re(B,e,M),Ur([B]);else if(M!=null){let X=function(oe){f[E].call(this,oe)};var V=X;f[D]=Ci(B,e,X,R)}}else if(E==="style")ve(e,E,M);else if(E==="autofocus")Xl(e,!!M);else if(!d&&(E==="__value"||E==="value"&&M!=null))e.value=e.__value=M;else if(E==="selected"&&y)Dd(e,M);else{var $=E;p||($=fd($));var J=$==="defaultValue"||$==="defaultChecked";if(M==null&&!d&&!J)if(l[E]=null,$==="value"||$==="checked"){let R=e;const D=t===void 0;if($==="value"){let B=R.defaultValue;R.removeAttribute($),R.defaultValue=B,R.value=R.__value=D?B:null}else{let B=R.defaultChecked;R.removeAttribute($),R.defaultChecked=B,R.checked=D?B:!1}}else e.removeAttribute(E);else J||x.includes($)&&(d||typeof M!="string")?(e[$]=M,$ in l&&(l[$]=Zt)):typeof M!="function"&&ve(e,$,M)}}}return f}function ko(e,t,r=[],n=[],s=[],i,l=!1,d=!1){ri(s,r,n,p=>{var f=void 0,y={},w=e.nodeName===Id,x=!1;if(gi(()=>{var A=t(...p.map(a)),N=jd(e,f,A,i,l,d);x&&w&&"value"in A&&Pn(e,A.value);for(let $ of Object.getOwnPropertySymbols(y))A[$]||dr(y[$]);for(let $ of Object.getOwnPropertySymbols(A)){var S=A[$];$.description===Sl&&(!f||S!==f[$])&&(y[$]&&dr(y[$]),y[$]=br(()=>Pd(e,()=>S))),N[$]=S}f=N}),w){var O=e;Ln(()=>{Pn(O,f.value,!0),eo(O)})}x=!0})}function to(e){return e.__attributes??(e.__attributes={[Ti]:e.nodeName.includes("-"),[Fi]:e.namespaceURI===qo})}var wo=new Map;function Ni(e){var t=e.getAttribute("is")||e.nodeName,r=wo.get(t);if(r)return r;wo.set(t,r=[]);for(var n,s=e,i=Element.prototype;i!==s;){n=Wi(s);for(var l in n)n[l].set&&r.push(l);s=jo(s)}return r}function Zr(e,t,r=t){var n=new WeakSet;Js(e,"input",async s=>{var i=s?e.defaultValue:e.value;if(i=hs(e)?ms(i):i,r(i),Te!==null&&n.add(Te),await Fs(),i!==(i=t())){var l=e.selectionStart,d=e.selectionEnd,p=e.value.length;if(e.value=i??"",d!==null){var f=e.value.length;l===d&&d===p&&f>p?(e.selectionStart=f,e.selectionEnd=f):(e.selectionStart=l,e.selectionEnd=Math.min(d,f))}}}),Ma(t)==null&&e.value&&(r(hs(e)?ms(e.value):e.value),Te!==null&&n.add(Te)),ls(()=>{var s=t();if(e===document.activeElement){var i=Gn??Te;if(n.has(i))return}hs(e)&&s===ms(e.value)||e.type==="date"&&!s&&!e.value||s!==e.value&&(e.value=s??"")})}function Hd(e,t,r=t){Js(e,"change",n=>{var s=n?e.defaultChecked:e.checked;r(s)}),Ma(t)==null&&r(e.checked),ls(()=>{var n=t();e.checked=!!n})}function hs(e){var t=e.type;return t==="number"||t==="range"}function ms(e){return e===""?null:+e}function So(e,t){return e===t||(e==null?void 0:e[xa])===t}function Rs(e={},t,r,n){return Ln(()=>{var s,i;return ls(()=>{s=i,i=[],Ma(()=>{e!==r(...i)&&(t(e,...i),s&&So(r(...s),e)&&t(null,...s))})}),()=>{Dr(()=>{i&&So(r(...i),e)&&t(null,...i)})}}),e}let Bn=!1;function Ud(e){var t=Bn;try{return Bn=!1,[e(),Bn]}finally{Bn=t}}const zd={get(e,t){if(!e.exclude.includes(t))return e.props[t]},set(e,t){return!1},getOwnPropertyDescriptor(e,t){if(!e.exclude.includes(t)&&t in e.props)return{enumerable:!0,configurable:!0,value:e.props[t]}},has(e,t){return e.exclude.includes(t)?!1:t in e.props},ownKeys(e){return Reflect.ownKeys(e.props).filter(t=>!e.exclude.includes(t))}};function ot(e,t,r){return new Proxy({props:e,exclude:t},zd)}const Vd={get(e,t){let r=e.props.length;for(;r--;){let n=e.props[r];if(Ka(n)&&(n=n()),typeof n=="object"&&n!==null&&t in n)return n[t]}},set(e,t,r){let n=e.props.length;for(;n--;){let s=e.props[n];Ka(s)&&(s=s());const i=ya(s,t);if(i&&i.set)return i.set(r),!0}return!1},getOwnPropertyDescriptor(e,t){let r=e.props.length;for(;r--;){let n=e.props[r];if(Ka(n)&&(n=n()),typeof n=="object"&&n!==null&&t in n){const s=ya(n,t);return s&&!s.configurable&&(s.configurable=!0),s}}},has(e,t){if(t===xa||t===Uo)return!1;for(let r of e.props)if(Ka(r)&&(r=r()),r!=null&&t in r)return!0;return!1},ownKeys(e){const t=[];for(let r of e.props)if(Ka(r)&&(r=r()),!!r){for(const n in r)t.includes(n)||t.push(n);for(const n of Object.getOwnPropertySymbols(r))t.includes(n)||t.push(n)}return t}};function lt(...e){return new Proxy({props:e},Vd)}function Wa(e,t,r,n){var J;var s=(r&bl)!==0,i=(r&yl)!==0,l=n,d=!0,p=()=>(d&&(d=!1,l=i?Ma(n):n),l),f;if(s){var y=xa in e||Uo in e;f=((J=ya(e,t))==null?void 0:J.set)??(y&&t in e?V=>e[t]=V:void 0)}var w,x=!1;s?[w,x]=Ud(()=>e[t]):w=e[t],w===void 0&&n!==void 0&&(w=p(),f&&(il(),f(w)));var O;if(O=()=>{var V=e[t];return V===void 0?p():(d=!0,V)},!(r&ml))return O;if(f){var A=e.$$legacy;return function(V,E){return arguments.length>0?((!E||A||x)&&f(E?O():V),V):O()}}var N=!1,S=(r&hl?os:ai)(()=>(N=!1,O()));s&&a(S);var $=nt;return function(V,E){if(arguments.length>0){const M=E?a(S):s?ut(V):V;return c(S,M),N=!0,l!==void 0&&(l=M),V}return Sa&&N||$.f&ea?S.v:a(S)}}function qd(e){yr===null&&zo(),lr(()=>{const t=Ma(e);if(typeof t=="function")return t})}function Bd(e){yr===null&&zo(),qd(()=>()=>Ma(e))}const Kd="5";var Do;typeof window<"u"&&((Do=window.__svelte??(window.__svelte={})).v??(Do.v=new Set)).add(Kd);const ro="prx-console-token",Wd=[{labelKey:"nav.overview",path:"/overview"},{labelKey:"nav.sessions",path:"/sessions"},{labelKey:"nav.channels",path:"/channels"},{labelKey:"nav.hooks",path:"/hooks"},{labelKey:"nav.mcp",path:"/mcp"},{labelKey:"nav.skills",path:"/skills"},{labelKey:"nav.plugins",path:"/plugins"},{labelKey:"nav.config",path:"/config"},{labelKey:"nav.logs",path:"/logs"}],$o="prx_console_token";function Jd(){const e=["Path=/","SameSite=Strict"];return typeof window<"u"&&window.location.protocol==="https:"&&e.push("Secure"),e.join("; ")}function ao(e){if(typeof document>"u")return;const t=e.trim();if(!t){document.cookie=`${$o}=; Path=/; Max-Age=0; SameSite=Strict`;return}document.cookie=`${$o}=${encodeURIComponent(t)}; ${Jd()}`}function Xn(){var e;return typeof window>"u"?"":((e=window.localStorage.getItem(ro))==null?void 0:e.trim())??""}function Gd(e){if(typeof window>"u")return;const t=e.trim();window.localStorage.setItem(ro,t),ao(t)}function Oi(){typeof window>"u"||(window.localStorage.removeItem(ro),ao(""))}function Mo(){ao(Xn())}const Qd={title:"PRX Console",menu:"Menu",closeSidebar:"Close sidebar",language:"Language",theme:"Theme",notFound:"Not found",backToOverview:"Back to Overview"},Yd={en:"English",zh:"中文",ka:"ქართული",ru:"Русский"},Xd={overview:"Overview",sessions:"Sessions",channels:"Channels",config:"Config",hooks:"Hooks",mcp:"MCP",skills:"Skills",plugins:"Plugins",logs:"Logs"},Zd={logout:"Logout",loading:"Loading...",error:"Error",refresh:"Refresh",updatedAt:"Updated {time}",na:"N/A",enabled:"Enabled",disabled:"Disabled",yes:"Yes",no:"No",unknown:"Unknown",clipboardUnavailable:"Clipboard not available.",copied:"Copied",copyFailed:"Copy failed",empty:"Empty",save:"Save",saving:"Saving...",reset:"Reset",reload:"Reload",discard:"Discard",add:"Add",visibilityToggle:"Toggle visibility",requestFailed:"Request failed ({status})",unauthorized:"Unauthorized",fileTypeUnknown:"Unknown",durationUnits:{day:"d",hour:"h",minute:"m",second:"s"},fileSizeUnits:{b:"B",kb:"KB",mb:"MB",gb:"GB"}},ec={title:"Overview",version:"Version",uptime:"Uptime",model:"Model",memoryBackend:"Memory Backend",gatewayPort:"Gateway Port",configuredChannels:"Configured Channels",loading:"Loading status...",loadFailed:"Failed to load status.",noChannelsConfigured:"No channels configured."},tc={title:"Sessions",searchPlaceholder:"Search session ID, sender, or message",allChannels:"All channels",applyFilters:"Apply",statusLabel:"Status",previousPage:"Previous",nextPage:"Next",pageLabel:"Page {page}",sessionId:"Session ID",sender:"Sender",channel:"Channel",messages:"Messages",lastMessage:"Last Message",loading:"Loading sessions...",loadFailed:"Failed to load sessions.",none:"No sessions found.",status:{all:"All statuses",active:"Active",pending:"Pending",empty:"Empty"}},rc={title:"Chat",session:"Session",back:"Back to Sessions",loading:"Loading messages...",loadFailed:"Failed to load messages.",sendFailed:"Failed to send message.",empty:"No messages in this session.",loadMore:"Load older messages",loadingMore:"Loading older messages...",messagesRegion:"Chat messages",dropFiles:"Drop files to attach ({count}/{max} selected)",attachments:"Attachments ({count}/{max})",removeAttachment:"Remove",attachFiles:"Attach files",attachmentAlt:"Attachment",inputPlaceholder:"Type a message...",send:"Send",sending:"Sending...",documentFallback:"DOC"},ac={title:"Channels",type:"Type",status:"Status",loading:"Loading channels...",loadFailed:"Failed to load channel status.",noChannels:"No channels available.",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"Email",irc:"IRC",lark:"Lark",dingtalk:"DingTalk",qq:"QQ",cli:"CLI",configured:"Configured"}},nc={title:"Config",rawJson:"Raw JSON",structured:"Structured View",copy:"Copy",copyJson:"Copy JSON",loading:"Loading config...",loadFailed:"Failed to load config.",description:"Schema-driven editor with defaults, search, and config file management.",advancedMode:"Advanced mode",mergedJsonTitle:"Merged JSON",mergedJsonDescription:"Direct editor for the merged runtime config payload.",configFilesTitle:"Config Files",configFilesDescription:"`config.toml` and `config.d/*.toml` are editable independently.",sourceMain:"config.toml",sourceDirectory:"config.d",searchPlaceholder:"Search by field name or description",noMatchingFields:"No matching fields.",noMatchingItems:"No matching config items.",toggleVisibility:"Toggle visibility",modified:"Modified",unsaved:"Unsaved",currentValue:"Current",defaultValue:"Default",noDefault:"No default",addListItem:"Add item",saveJson:"Save JSON",saveFile:"Save file",saveConfig:"Save config",discard:"Discard",unsavedChangesCount:"{count} unsaved change(s)",saveHint:"Save writes only changed keys back to the config API.",section:{general:"General",gateway:"Gateway",channels:"Channels",memory:"Memory",security:"Security",model:"Model",other:"Other"},field:{version:"Version",runtimeModel:"Runtime Model",memoryBackend:"Memory Backend",configuredChannels:"Configured Channels",notConfigured:"Not configured",notSet:"Not set"},channel:{settings:"settings",notConfigured:"Not configured"},redacted:"Redacted",emptyObject:"No settings",saveSuccess:"Saved.",saveRestartRequired:"Saved. Some settings require a restart to take effect.",saveFailed:"Save failed: {message}"},sc={title:"Logs",connected:"Connected",disconnected:"Disconnected",reconnecting:"Reconnecting",pause:"Pause",resume:"Resume",clear:"Clear",waiting:"Waiting for log stream..."},oc={title:"Hooks",loading:"Loading hooks...",loadFailed:"Failed to load hooks.",noHooks:"No hooks configured.",globalStatus:"Global enabled state",addHook:"Add Hook",cancelAdd:"Cancel",newHook:"New Hook",event:"Event",command:"Command",commandPlaceholder:"e.g. /opt/scripts/on-event.sh",timeout:"Timeout (ms)",enabled:"Enabled",globalToggleHint:"Enabled state is currently controlled globally by the backend.",edit:"Edit",delete:"Delete",deleting:"Deleting...",save:"Save",saving:"Saving...",cancel:"Cancel",commandRequired:"Command is required.",timeoutInvalid:"Timeout must be at least 1000 ms.",saveFailed:"Failed to save hook.",deleteFailed:"Failed to delete hook.",toggleFailed:"Failed to update hook state.",events:{agent_start:"Agent start",agent_end:"Agent end",llm_request:"LLM request",llm_response:"LLM response",tool_call_start:"Tool call start",tool_call:"Tool call",turn_complete:"Turn complete",error:"Error"}},ic={title:"MCP Servers",loading:"Loading MCP servers...",loadFailed:"Failed to load MCP servers.",noServers:"No MCP servers configured.",connected:"Connected",connecting:"Connecting",disconnected:"Disconnected",tools:"tools",availableTools:"Available Tools",noTools:"No tools available."},lc={title:"Skills",loading:"Loading skills...",noSkills:"No skills installed.",active:"active",tabInstalled:"Installed",tabDiscover:"Discover",search:"Search skills...",source:"Source",searchBtn:"Search",searching:"Searching...",loadFailed:"Failed to load skills.",searchFailed:"Failed to search skills.",noResults:"No results found.",install:"Install",installing:"Installing...",installed:"Installed",uninstall:"Uninstall",uninstalling:"Removing...",confirmUninstall:'Are you sure you want to uninstall "{name}"?',stars:"stars",owner:"by",licensed:"Licensed",unlicensed:"No license",readOnlyState:"Enable state is read-only.",installSuccess:"Skill installed successfully",installFailed:"Failed to install skill",uninstallSuccess:"Skill uninstalled",uninstallFailed:"Failed to uninstall skill",sources:{github:"GitHub"}},dc={title:"Plugins",loading:"Loading plugins...",loadFailed:"Failed to load plugins.",noPlugins:"No WASM plugins loaded.",capabilities:"Capabilities",permissions:"Permissions",statusActive:"Active",reload:"Reload",reloadSuccess:'Plugin "{name}" reloaded',reloadFailed:"Failed to reload plugin"},cc={title:"PRX Console Login",accessToken:"Access Token",login:"Login",hint:"Enter your gateway auth token to continue.",placeholder:"Bearer token",tokenRequired:"Access token is required."},uc={app:Qd,languages:Yd,nav:Xd,common:Zd,overview:ec,sessions:tc,chat:rc,channels:ac,config:nc,logs:sc,hooks:oc,mcp:ic,skills:lc,plugins:dc,login:cc},fc={title:"PRX Console",menu:"მენიუ",closeSidebar:"გვერდითი პანელის დახურვა",language:"ენა",theme:"თემა",notFound:"ვერ მოიძებნა",backToOverview:"მიმოხილვაზე დაბრუნება"},vc={en:"English",zh:"中文",ka:"ქართული",ru:"Русский"},gc={overview:"მიმოხილვა",sessions:"სესიები",channels:"არხები",config:"კონფიგურაცია",hooks:"ჰუკები",mcp:"MCP",skills:"უნარები",plugins:"პლაგინები",logs:"ლოგები"},pc={logout:"გასვლა",loading:"იტვირთება...",error:"შეცდომა",refresh:"განახლება",updatedAt:"განახლდა {time}",na:"არ არის",enabled:"ჩართულია",disabled:"გამორთულია",yes:"დიახ",no:"არა",unknown:"უცნობია",clipboardUnavailable:"გაცვლის ბუფერი მიუწვდომელია.",copied:"დაკოპირდა",copyFailed:"კოპირება ვერ მოხერხდა",empty:"ცარიელია",save:"შენახვა",saving:"ინახება...",reset:"ჩამოყრა",reload:"თავიდან ჩატვირთვა",discard:"გაუქმება",add:"დამატება",visibilityToggle:"ხილვადობის გადართვა",requestFailed:"მოთხოვნა ვერ შესრულდა ({status})",unauthorized:"ავტორიზაცია არ არის",fileTypeUnknown:"უცნობია",durationUnits:{day:"დღ",hour:"სთ",minute:"წთ",second:"წმ"},fileSizeUnits:{b:"B",kb:"KB",mb:"MB",gb:"GB"}},hc={title:"მიმოხილვა",version:"ვერსია",uptime:"მუშაობის დრო",model:"მოდელი",memoryBackend:"მეხსიერების ბექენდი",gatewayPort:"გეითვეის პორტი",configuredChannels:"კონფიგურირებული არხები",loading:"სტატუსი იტვირთება...",loadFailed:"სტატუსის ჩატვირთვა ვერ მოხერხდა.",noChannelsConfigured:"არხები არ არის კონფიგურირებული."},mc={title:"სესიები",searchPlaceholder:"ძიება სესიის ID-ით, გამგზავნით ან შეტყობინებით",allChannels:"ყველა არხი",applyFilters:"გამოყენება",statusLabel:"სტატუსი",previousPage:"წინა",nextPage:"შემდეგი",pageLabel:"გვერდი {page}",sessionId:"სესიის ID",sender:"გამგზავნი",channel:"არხი",messages:"შეტყობინებები",lastMessage:"ბოლო შეტყობინება",loading:"სესიები იტვირთება...",loadFailed:"სესიების ჩატვირთვა ვერ მოხერხდა.",none:"სესიები ვერ მოიძებნა.",status:{all:"ყველა სტატუსი",active:"აქტიური",pending:"მოლოდინში",empty:"ცარიელი"}},bc={title:"ჩატი",session:"სესია",back:"სესიებზე დაბრუნება",loading:"შეტყობინებები იტვირთება...",loadFailed:"შეტყობინებების ჩატვირთვა ვერ მოხერხდა.",sendFailed:"შეტყობინების გაგზავნა ვერ მოხერხდა.",empty:"ამ სესიაში შეტყობინებები არ არის.",loadMore:"ძველი შეტყობინებების ჩატვირთვა",loadingMore:"ძველი შეტყობინებები იტვირთება...",messagesRegion:"ჩატის შეტყობინებები",dropFiles:"ჩააგდეთ ფაილები მისამაგრებლად ({count}/{max} არჩეულია)",attachments:"მიმაგრებული ფაილები ({count}/{max})",removeAttachment:"წაშლა",attachFiles:"ფაილების მიმაგრება",attachmentAlt:"მიმაგრებული ფაილი",inputPlaceholder:"შეიყვანეთ შეტყობინება...",send:"გაგზავნა",sending:"იგზავნება...",documentFallback:"დოკ"},yc={title:"არხები",type:"ტიპი",status:"სტატუსი",loading:"არხები იტვირთება...",loadFailed:"არხების სტატუსის ჩატვირთვა ვერ მოხერხდა.",noChannels:"არხები არ არის ხელმისაწვდომი.",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"Email",irc:"IRC",lark:"Lark",dingtalk:"DingTalk",qq:"QQ",cli:"CLI",configured:"კონფიგურირებულია"}},_c={title:"კონფიგურაცია",rawJson:"Raw JSON",structured:"სტრუქტურირებული ხედი",copy:"კოპირება",copyJson:"JSON-ის კოპირება",loading:"კონფიგურაცია იტვირთება...",loadFailed:"კონფიგურაციის ჩატვირთვა ვერ მოხერხდა.",description:"სქემაზე დაფუძნებული რედაქტორი ნაგულისხმევი მნიშვნელობებით, ძიებით და კონფიგურაციის ფაილების მართვით.",advancedMode:"გაფართოებული რეჟიმი",mergedJsonTitle:"შერწყმული JSON",mergedJsonDescription:"გაერთიანებული runtime კონფიგურაციის პირდაპირი რედაქტორი.",configFilesTitle:"კონფიგურაციის ფაილები",configFilesDescription:"`config.toml` და `config.d/*.toml` დამოუკიდებლად რედაქტირდება.",sourceMain:"config.toml",sourceDirectory:"config.d",searchPlaceholder:"ძიება ველის სახელით ან აღწერით",noMatchingFields:"შესაბამისი ველები ვერ მოიძებნა.",noMatchingItems:"შესაბამისი კონფიგურაციის ელემენტები ვერ მოიძებნა.",toggleVisibility:"ხილვადობის გადართვა",modified:"შეცვლილია",unsaved:"შეუნახავი",currentValue:"მიმდინარე",defaultValue:"ნაგულისხმევი",noDefault:"ნაგულისხმევი მნიშვნელობა არ არის",addListItem:"ელემენტის დამატება",saveJson:"JSON-ის შენახვა",saveFile:"ფაილის შენახვა",saveConfig:"კონფიგურაციის შენახვა",discard:"გაუქმება",unsavedChangesCount:"{count} შეუნახავი ცვლილება",saveHint:"შენახვისას API-ში ბრუნდება მხოლოდ შეცვლილი გასაღებები.",section:{general:"ზოგადი",gateway:"Gateway",channels:"არხები",memory:"მეხსიერება",security:"უსაფრთხოება",model:"მოდელი",other:"სხვა"},field:{version:"ვერსია",runtimeModel:"Runtime მოდელი",memoryBackend:"მეხსიერების ბექენდი",configuredChannels:"კონფიგურირებული არხები",notConfigured:"არ არის კონფიგურირებული",notSet:"არ არის დაყენებული"},channel:{settings:"პარამეტრები",notConfigured:"არ არის კონფიგურირებული"},redacted:"დაფარულია",emptyObject:"პარამეტრები არ არის",saveSuccess:"შენახულია.",saveRestartRequired:"შენახულია. ზოგიერთი პარამეტრი ძალაში შესასვლელად გადატვირთვას საჭიროებს.",saveFailed:"შენახვა ვერ მოხერხდა: {message}"},xc={title:"ლოგები",connected:"დაკავშირებულია",disconnected:"გათიშულია",reconnecting:"ხელახლა უკავშირდება",pause:"შეჩერება",resume:"გაგრძელება",clear:"გასუფთავება",waiting:"ლოგების ნაკადს ველოდებით..."},kc={title:"ჰუკები",loading:"ჰუკები იტვირთება...",loadFailed:"ჰუკების ჩატვირთვა ვერ მოხერხდა.",noHooks:"ჰუკები არ არის კონფიგურირებული.",globalStatus:"გლობალური ჩართვის მდგომარეობა",addHook:"ჰუკის დამატება",cancelAdd:"გაუქმება",newHook:"ახალი ჰუკი",event:"მოვლენა",command:"ბრძანება",commandPlaceholder:"მაგ. /opt/scripts/on-event.sh",timeout:"ტაიმაუტი (მს)",enabled:"ჩართულია",globalToggleHint:"ჩართვის მდგომარეობა ამჟამად გლობალურად კონტროლდება ბექენდის მიერ.",edit:"რედაქტირება",delete:"წაშლა",deleting:"იშლება...",save:"შენახვა",saving:"ინახება...",cancel:"გაუქმება",commandRequired:"ბრძანება აუცილებელია.",timeoutInvalid:"ტაიმაუტი მინიმუმ 1000 მს უნდა იყოს.",saveFailed:"ჰუკის შენახვა ვერ მოხერხდა.",deleteFailed:"ჰუკის წაშლა ვერ მოხერხდა.",toggleFailed:"ჰუკის მდგომარეობის განახლება ვერ მოხერხდა.",events:{agent_start:"აგენტის გაშვება",agent_end:"აგენტის დასრულება",llm_request:"LLM მოთხოვნა",llm_response:"LLM პასუხი",tool_call_start:"ინსტრუმენტის გამოძახების დაწყება",tool_call:"ინსტრუმენტის გამოძახება",turn_complete:"ტურის დასრულება",error:"შეცდომა"}},wc={title:"MCP სერვერები",loading:"MCP სერვერები იტვირთება...",loadFailed:"MCP სერვერების ჩატვირთვა ვერ მოხერხდა.",noServers:"MCP სერვერები არ არის კონფიგურირებული.",connected:"დაკავშირებულია",connecting:"უკავშირდება",disconnected:"გათიშულია",tools:"ინსტრუმენტი",availableTools:"ხელმისაწვდომი ინსტრუმენტები",noTools:"ინსტრუმენტები არ არის ხელმისაწვდომი."},Sc={title:"უნარები",loading:"უნარები იტვირთება...",noSkills:"უნარები არ არის დაყენებული.",active:"აქტიური",tabInstalled:"დაყენებული",tabDiscover:"მოძებნა",search:"უნარების ძიება...",source:"წყარო",searchBtn:"ძებნა",searching:"იძებნება...",loadFailed:"უნარების ჩატვირთვა ვერ მოხერხდა.",searchFailed:"უნარების ძიება ვერ მოხერხდა.",noResults:"შედეგები ვერ მოიძებნა.",install:"დაყენება",installing:"ყენდება...",installed:"დაყენებულია",uninstall:"წაშლა",uninstalling:"იშლება...",confirmUninstall:'ნამდვილად გსურთ "{name}"-ის წაშლა?',stars:"ვარსკვლავი",owner:"ავტორი",licensed:"ლიცენზირებულია",unlicensed:"ლიცენზია არ არის",readOnlyState:"ჩართვის მდგომარეობა მხოლოდ წასაკითხადაა.",installSuccess:"უნარი წარმატებით დაყენდა",installFailed:"უნარის დაყენება ვერ მოხერხდა",uninstallSuccess:"უნარი წაიშალა",uninstallFailed:"უნარის წაშლა ვერ მოხერხდა",sources:{github:"GitHub"}},$c={title:"პლაგინები",loading:"პლაგინები იტვირთება...",loadFailed:"პლაგინების ჩატვირთვა ვერ მოხერხდა.",noPlugins:"WASM პლაგინები არ არის ჩატვირთული.",capabilities:"შესაძლებლობები",permissions:"უფლებები",statusActive:"აქტიური",reload:"გადატვირთვა",reloadSuccess:'პლაგინი "{name}" გადაიტვირთა',reloadFailed:"პლაგინის გადატვირთვა ვერ მოხერხდა"},Mc={title:"PRX Console შესვლა",accessToken:"წვდომის ტოკენი",login:"შესვლა",hint:"გასაგრძელებლად შეიყვანეთ gateway-ის ავტორიზაციის ტოკენი.",placeholder:"Bearer ტოკენი",tokenRequired:"წვდომის ტოკენი აუცილებელია."},Cc={app:fc,languages:vc,nav:gc,common:pc,overview:hc,sessions:mc,chat:bc,channels:yc,config:_c,logs:xc,hooks:kc,mcp:wc,skills:Sc,plugins:$c,login:Mc},Ac={title:"PRX Console",menu:"Меню",closeSidebar:"Закрыть боковую панель",language:"Язык",theme:"Тема",notFound:"Не найдено",backToOverview:"Назад к обзору"},Ec={en:"English",zh:"中文",ka:"ქართული",ru:"Русский"},Pc={overview:"Обзор",sessions:"Сессии",channels:"Каналы",config:"Конфигурация",hooks:"Хуки",mcp:"MCP",skills:"Навыки",plugins:"Плагины",logs:"Логи"},Tc={logout:"Выйти",loading:"Загрузка...",error:"Ошибка",refresh:"Обновить",updatedAt:"Обновлено {time}",na:"Н/Д",enabled:"Включено",disabled:"Выключено",yes:"Да",no:"Нет",unknown:"Неизвестно",clipboardUnavailable:"Буфер обмена недоступен.",copied:"Скопировано",copyFailed:"Не удалось скопировать",empty:"Пусто",save:"Сохранить",saving:"Сохранение...",reset:"Сбросить",reload:"Перезагрузить",discard:"Отменить",add:"Добавить",visibilityToggle:"Переключить видимость",requestFailed:"Запрос завершился ошибкой ({status})",unauthorized:"Нет авторизации",fileTypeUnknown:"Неизвестно",durationUnits:{day:"д",hour:"ч",minute:"м",second:"с"},fileSizeUnits:{b:"B",kb:"KB",mb:"MB",gb:"GB"}},Fc={title:"Обзор",version:"Версия",uptime:"Время работы",model:"Модель",memoryBackend:"Бэкенд памяти",gatewayPort:"Порт шлюза",configuredChannels:"Настроенные каналы",loading:"Загрузка статуса...",loadFailed:"Не удалось загрузить статус.",noChannelsConfigured:"Каналы не настроены."},Nc={title:"Сессии",searchPlaceholder:"Поиск по ID сессии, отправителю или сообщению",allChannels:"Все каналы",applyFilters:"Применить",statusLabel:"Статус",previousPage:"Назад",nextPage:"Вперед",pageLabel:"Страница {page}",sessionId:"ID сессии",sender:"Отправитель",channel:"Канал",messages:"Сообщения",lastMessage:"Последнее сообщение",loading:"Загрузка сессий...",loadFailed:"Не удалось загрузить сессии.",none:"Сессии не найдены.",status:{all:"Все статусы",active:"Активна",pending:"В ожидании",empty:"Пустая"}},Oc={title:"Чат",session:"Сессия",back:"Назад к сессиям",loading:"Загрузка сообщений...",loadFailed:"Не удалось загрузить сообщения.",sendFailed:"Не удалось отправить сообщение.",empty:"В этой сессии нет сообщений.",loadMore:"Загрузить более старые сообщения",loadingMore:"Загрузка более старых сообщений...",messagesRegion:"Сообщения чата",dropFiles:"Перетащите файлы для прикрепления ({count}/{max} выбрано)",attachments:"Вложения ({count}/{max})",removeAttachment:"Удалить",attachFiles:"Прикрепить файлы",attachmentAlt:"Вложение",inputPlaceholder:"Введите сообщение...",send:"Отправить",sending:"Отправка...",documentFallback:"Док"},Lc={title:"Каналы",type:"Тип",status:"Статус",loading:"Загрузка каналов...",loadFailed:"Не удалось загрузить статус каналов.",noChannels:"Каналы недоступны.",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"Email",irc:"IRC",lark:"Lark",dingtalk:"DingTalk",qq:"QQ",cli:"CLI",configured:"Настроено"}},Ic={title:"Конфигурация",rawJson:"Raw JSON",structured:"Структурированный вид",copy:"Копировать",copyJson:"Копировать JSON",loading:"Загрузка конфигурации...",loadFailed:"Не удалось загрузить конфигурацию.",description:"Редактор на основе схемы с настройками по умолчанию, поиском и управлением конфигурационными файлами.",advancedMode:"Расширенный режим",mergedJsonTitle:"Объединенный JSON",mergedJsonDescription:"Прямой редактор объединенной runtime-конфигурации.",configFilesTitle:"Файлы конфигурации",configFilesDescription:"`config.toml` и `config.d/*.toml` редактируются независимо.",sourceMain:"config.toml",sourceDirectory:"config.d",searchPlaceholder:"Поиск по имени поля или описанию",noMatchingFields:"Подходящие поля не найдены.",noMatchingItems:"Подходящие элементы конфигурации не найдены.",toggleVisibility:"Переключить видимость",modified:"Изменено",unsaved:"Не сохранено",currentValue:"Текущее",defaultValue:"По умолчанию",noDefault:"Нет значения по умолчанию",addListItem:"Добавить элемент",saveJson:"Сохранить JSON",saveFile:"Сохранить файл",saveConfig:"Сохранить конфигурацию",discard:"Отменить",unsavedChangesCount:"{count} несохраненных изменений",saveHint:"При сохранении в API отправляются только измененные ключи.",section:{general:"Общие",gateway:"Gateway",channels:"Каналы",memory:"Память",security:"Безопасность",model:"Модель",other:"Другое"},field:{version:"Версия",runtimeModel:"Runtime-модель",memoryBackend:"Бэкенд памяти",configuredChannels:"Настроенные каналы",notConfigured:"Не настроено",notSet:"Не задано"},channel:{settings:"настройки",notConfigured:"Не настроено"},redacted:"Скрыто",emptyObject:"Нет настроек",saveSuccess:"Сохранено.",saveRestartRequired:"Сохранено. Некоторые настройки вступят в силу после перезапуска.",saveFailed:"Ошибка сохранения: {message}"},Rc={title:"Логи",connected:"Подключено",disconnected:"Отключено",reconnecting:"Переподключение",pause:"Пауза",resume:"Продолжить",clear:"Очистить",waiting:"Ожидание потока логов..."},Dc={title:"Хуки",loading:"Загрузка хуков...",loadFailed:"Не удалось загрузить хуки.",noHooks:"Хуки не настроены.",globalStatus:"Глобальное состояние включения",addHook:"Добавить хук",cancelAdd:"Отмена",newHook:"Новый хук",event:"Событие",command:"Команда",commandPlaceholder:"например /opt/scripts/on-event.sh",timeout:"Таймаут (мс)",enabled:"Включено",globalToggleHint:"Состояние включения сейчас глобально управляется бэкендом.",edit:"Изменить",delete:"Удалить",deleting:"Удаление...",save:"Сохранить",saving:"Сохранение...",cancel:"Отмена",commandRequired:"Команда обязательна.",timeoutInvalid:"Таймаут должен быть не менее 1000 мс.",saveFailed:"Не удалось сохранить хук.",deleteFailed:"Не удалось удалить хук.",toggleFailed:"Не удалось обновить состояние хука.",events:{agent_start:"Запуск агента",agent_end:"Завершение агента",llm_request:"Запрос к LLM",llm_response:"Ответ LLM",tool_call_start:"Начало вызова инструмента",tool_call:"Вызов инструмента",turn_complete:"Ход завершен",error:"Ошибка"}},jc={title:"MCP-серверы",loading:"Загрузка MCP-серверов...",loadFailed:"Не удалось загрузить MCP-серверы.",noServers:"MCP-серверы не настроены.",connected:"Подключено",connecting:"Подключение",disconnected:"Отключено",tools:"инструментов",availableTools:"Доступные инструменты",noTools:"Инструменты недоступны."},Hc={title:"Навыки",loading:"Загрузка навыков...",noSkills:"Навыки не установлены.",active:"активно",tabInstalled:"Установленные",tabDiscover:"Поиск",search:"Поиск навыков...",source:"Источник",searchBtn:"Искать",searching:"Поиск...",loadFailed:"Не удалось загрузить навыки.",searchFailed:"Не удалось найти навыки.",noResults:"Результаты не найдены.",install:"Установить",installing:"Установка...",installed:"Установлено",uninstall:"Удалить",uninstalling:"Удаление...",confirmUninstall:'Вы уверены, что хотите удалить "{name}"?',stars:"звезды",owner:"автор",licensed:"Лицензировано",unlicensed:"Без лицензии",readOnlyState:"Состояние включения доступно только для чтения.",installSuccess:"Навык успешно установлен",installFailed:"Не удалось установить навык",uninstallSuccess:"Навык удален",uninstallFailed:"Не удалось удалить навык",sources:{github:"GitHub"}},Uc={title:"Плагины",loading:"Загрузка плагинов...",loadFailed:"Не удалось загрузить плагины.",noPlugins:"WASM-плагины не загружены.",capabilities:"Возможности",permissions:"Разрешения",statusActive:"Активен",reload:"Перезагрузить",reloadSuccess:'Плагин "{name}" перезагружен',reloadFailed:"Не удалось перезагрузить плагин"},zc={title:"Вход в PRX Console",accessToken:"Токен доступа",login:"Войти",hint:"Введите токен авторизации gateway, чтобы продолжить.",placeholder:"Bearer token",tokenRequired:"Токен доступа обязателен."},Vc={app:Ac,languages:Ec,nav:Pc,common:Tc,overview:Fc,sessions:Nc,chat:Oc,channels:Lc,config:Ic,logs:Rc,hooks:Dc,mcp:jc,skills:Hc,plugins:Uc,login:zc},qc={title:"PRX 控制台",menu:"菜单",closeSidebar:"关闭侧边栏",language:"语言",theme:"主题",notFound:"页面未找到",backToOverview:"返回概览"},Bc={en:"English",zh:"中文",ka:"ქართული",ru:"Русский"},Kc={overview:"概览",sessions:"会话",channels:"通道",config:"配置",hooks:"Hooks",mcp:"MCP",skills:"Skills",plugins:"插件",logs:"日志"},Wc={logout:"退出登录",loading:"加载中...",error:"错误",refresh:"刷新",updatedAt:"更新时间 {time}",na:"暂无",enabled:"已启用",disabled:"已禁用",yes:"是",no:"否",unknown:"未知",clipboardUnavailable:"当前环境不支持剪贴板。",copied:"已复制",copyFailed:"复制失败",empty:"空",save:"保存",saving:"保存中...",reset:"重置",reload:"重新加载",discard:"放弃更改",add:"添加",visibilityToggle:"切换可见性",requestFailed:"请求失败（{status}）",unauthorized:"未授权",fileTypeUnknown:"未知",durationUnits:{day:"天",hour:"小时",minute:"分",second:"秒"},fileSizeUnits:{b:"B",kb:"KB",mb:"MB",gb:"GB"}},Jc={title:"概览",version:"版本",uptime:"运行时长",model:"模型",memoryBackend:"记忆后端",gatewayPort:"网关端口",configuredChannels:"已配置通道",loading:"正在加载状态...",loadFailed:"加载状态失败。",noChannelsConfigured:"尚未配置任何通道。"},Gc={title:"会话",searchPlaceholder:"搜索会话 ID、发送方或消息内容",allChannels:"全部通道",applyFilters:"应用",statusLabel:"状态",previousPage:"上一页",nextPage:"下一页",pageLabel:"第 {page} 页",sessionId:"会话 ID",sender:"发送方",channel:"通道",messages:"消息数",lastMessage:"最后消息",loading:"正在加载会话...",loadFailed:"加载会话失败。",none:"未找到会话。",status:{all:"全部状态",active:"活跃",pending:"待处理",empty:"空"}},Qc={title:"聊天",session:"会话",back:"返回会话列表",loading:"正在加载消息...",loadFailed:"加载消息失败。",sendFailed:"发送消息失败。",empty:"此会话暂无消息。",loadMore:"加载更早消息",loadingMore:"正在加载更早消息...",messagesRegion:"聊天消息",dropFiles:"拖放文件以上传（已选 {count}/{max}）",attachments:"附件（{count}/{max}）",removeAttachment:"移除",attachFiles:"添加附件",attachmentAlt:"附件",inputPlaceholder:"输入消息...",send:"发送",sending:"发送中...",documentFallback:"文档"},Yc={title:"通道",type:"类型",status:"状态",loading:"正在加载通道状态...",loadFailed:"加载通道状态失败。",noChannels:"暂无通道数据。",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"邮件",irc:"IRC",lark:"飞书",dingtalk:"钉钉",qq:"QQ",cli:"命令行",configured:"已配置"}},Xc={title:"配置",rawJson:"原始 JSON",structured:"结构化视图",copy:"复制",copyJson:"复制 JSON",loading:"正在加载配置...",loadFailed:"加载配置失败。",description:"基于 schema 的配置编辑器，支持默认值、搜索和配置文件管理。",advancedMode:"高级模式",mergedJsonTitle:"合并后的 JSON",mergedJsonDescription:"直接编辑合并后的运行时配置。",configFilesTitle:"配置文件",configFilesDescription:"`config.toml` 和 `config.d/*.toml` 可独立编辑。",sourceMain:"config.toml",sourceDirectory:"config.d",searchPlaceholder:"按字段名或描述搜索",noMatchingFields:"未找到匹配字段。",noMatchingItems:"未找到匹配配置项。",toggleVisibility:"切换可见性",modified:"已修改",unsaved:"未保存",currentValue:"当前值",defaultValue:"默认值",noDefault:"无默认值",addListItem:"添加条目",saveJson:"保存 JSON",saveFile:"保存文件",saveConfig:"保存配置",discard:"放弃更改",unsavedChangesCount:"{count} 项未保存更改",saveHint:"保存时仅会将变更的键写回配置 API。",section:{general:"常规",gateway:"网关",channels:"通道",memory:"记忆",security:"安全",model:"模型",other:"其他"},field:{version:"版本",runtimeModel:"运行模型",memoryBackend:"记忆后端",configuredChannels:"已配置通道",notConfigured:"未配置",notSet:"未设置"},channel:{settings:"配置",notConfigured:"未配置"},redacted:"已脱敏",emptyObject:"无配置项",saveSuccess:"已保存。",saveRestartRequired:"已保存，部分设置需要重启服务后生效。",saveFailed:"保存失败：{message}"},Zc={title:"日志",connected:"已连接",disconnected:"已断开",reconnecting:"重连中",pause:"暂停",resume:"继续",clear:"清空",waiting:"等待日志流..."},eu={title:"Hooks",loading:"正在加载 Hooks...",loadFailed:"加载 Hooks 失败。",noHooks:"尚未配置任何 Hook。",globalStatus:"全局启用状态",addHook:"添加 Hook",cancelAdd:"取消",newHook:"新建 Hook",event:"事件",command:"命令",commandPlaceholder:"例如 /opt/scripts/on-event.sh",timeout:"超时 (ms)",enabled:"启用",globalToggleHint:"当前启用状态由后端全局控制。",edit:"编辑",delete:"删除",deleting:"删除中...",save:"保存",saving:"保存中...",cancel:"取消",commandRequired:"命令不能为空。",timeoutInvalid:"超时必须至少为 1000 毫秒。",saveFailed:"保存 Hook 失败。",deleteFailed:"删除 Hook 失败。",toggleFailed:"更新 Hook 状态失败。",events:{agent_start:"Agent 启动",agent_end:"Agent 结束",llm_request:"LLM 请求",llm_response:"LLM 响应",tool_call_start:"工具调用开始",tool_call:"工具调用",turn_complete:"轮次完成",error:"错误"}},tu={title:"MCP 服务",loading:"正在加载 MCP 服务...",loadFailed:"加载 MCP 服务失败。",noServers:"尚未配置任何 MCP 服务。",connected:"已连接",connecting:"连接中",disconnected:"已断开",tools:"个工具",availableTools:"可用工具",noTools:"无可用工具。"},ru={title:"Skills",loading:"正在加载 Skills...",noSkills:"尚未安装任何 Skill。",active:"已启用",tabInstalled:"已安装",tabDiscover:"发现新 Skills",search:"搜索 Skills...",source:"来源",searchBtn:"搜索",searching:"搜索中...",loadFailed:"加载 Skills 失败。",searchFailed:"搜索 Skill 失败。",noResults:"未找到结果。",install:"安装",installing:"安装中...",installed:"已安装",uninstall:"卸载",uninstalling:"卸载中...",confirmUninstall:'确定要卸载 "{name}" 吗？',stars:"星标",owner:"作者",licensed:"有许可证",unlicensed:"无许可证",readOnlyState:"启用状态当前为只读展示。",installSuccess:"Skill 安装成功",installFailed:"Skill 安装失败",uninstallSuccess:"Skill 已卸载",uninstallFailed:"Skill 卸载失败",sources:{github:"GitHub"}},au={title:"插件",loading:"正在加载插件...",loadFailed:"加载插件失败。",noPlugins:"未加载任何 WASM 插件。",capabilities:"能力",permissions:"权限",statusActive:"运行中",reload:"重载",reloadSuccess:'插件 "{name}" 已重载',reloadFailed:"插件重载失败"},nu={title:"PRX 控制台登录",accessToken:"访问令牌",login:"登录",hint:"请输入网关认证令牌以继续。",placeholder:"Bearer 令牌",tokenRequired:"访问令牌不能为空。"},su={app:qc,languages:Bc,nav:Kc,common:Wc,overview:Jc,sessions:Gc,chat:Qc,channels:Yc,config:Xc,logs:Zc,hooks:eu,mcp:tu,skills:ru,plugins:au,login:nu},cs="prx-console-lang",no=["en","zh","ka","ru"],Tn="en",bs={en:uc,zh:su,ka:Cc,ru:Vc};function Ds(e){if(typeof e!="string"||e.trim().length===0)return Tn;const t=e.trim().toLowerCase();return t.startsWith("zh")?"zh":t.startsWith("ka")?"ka":t.startsWith("ru")?"ru":no.includes(t)?t:"en"}function ou(){var e;if(typeof window<"u"){const t=window.localStorage.getItem(cs);if(t)return Ds(t)}if(typeof navigator<"u"){const t=navigator.language||((e=navigator.languages)==null?void 0:e[0])||Tn;return Ds(t)}return Tn}function Co(e,t){return t.split(".").reduce((r,n)=>{if(!(!r||typeof r!="object"))return r[n]},e)}function Li(e){typeof document<"u"&&(document.documentElement.lang=e==="zh"?"zh-CN":e==="ka"?"ka-GE":e==="ru"?"ru-RU":"en")}function iu(e){typeof window<"u"&&window.localStorage.setItem(cs,e)}const $a=ut({lang:ou()});Li($a.lang);function so(e){const t=Ds(e);$a.lang!==t&&($a.lang=t,iu(t),Li(t))}function lu(){if(typeof window>"u")return;const e=window.localStorage.getItem(cs);e&&so(e)}function u(e,t={}){const r=bs[$a.lang]??bs[Tn];let n=Co(r,e);if(typeof n!="string"&&(n=Co(bs[Tn],e)),typeof n!="string")return e;for(const[s,i]of Object.entries(t))n=n.replaceAll(`{${s}}`,String(i));return n}function Ii(){return typeof window>"u"?"/":window.location.pathname||"/"}function ga(e,t=!1){if(typeof window>"u")return;e.startsWith("/")||(e=`/${e}`);const r=t?"replaceState":"pushState";window.location.pathname!==e&&(window.history[r]({},"",e),window.dispatchEvent(new PopStateEvent("popstate")))}function du(e){if(typeof window>"u")return()=>{};const t=()=>{e(Ii())};return window.addEventListener("popstate",t),t(),()=>{window.removeEventListener("popstate",t)}}const ys="".trim(),Zn=ys.endsWith("/")?ys.slice(0,-1):ys;class Ao extends Error{constructor(t,r){super(r),this.name="ApiError",this.status=t}}async function cu(e){return(e.headers.get("content-type")||"").includes("application/json")?e.json().catch(()=>null):e.text().catch(()=>null)}function uu(e,t){return e&&typeof e=="object"&&typeof e.error=="string"?e.error:u("common.requestFailed",{status:t})}async function Ft(e,t={}){const r=Xn(),n={Accept:"application/json",...t.headers};r&&(n.Authorization=`Bearer ${r}`),t.body&&!(t.body instanceof FormData)&&!n["Content-Type"]&&(n["Content-Type"]="application/json");const s=await fetch(`${Zn}${e}`,{...t,credentials:t.credentials??"include",headers:n}),i=await cu(s);if(s.status===401)throw Oi(),ga("/",!0),new Ao(401,u("common.unauthorized"));if(!s.ok)throw new Ao(s.status,uu(i,s.status));return i}const St={getStatus:()=>Ft("/api/status"),getSessions:({limit:e,offset:t,channel:r,status:n,search:s}={})=>{const i=new URLSearchParams;e&&i.set("limit",String(e)),t&&i.set("offset",String(t)),r&&i.set("channel",r),n&&i.set("status",n),s&&i.set("search",s);const l=i.size>0?`?${i.toString()}`:"";return Ft(`/api/sessions${l}`)},getSessionMessages:(e,{limit:t,offset:r}={})=>{const n=new URLSearchParams;t&&n.set("limit",String(t)),r&&n.set("offset",String(r));const s=n.size>0?`?${n.toString()}`:"";return Ft(`/api/sessions/${encodeURIComponent(e)}/messages${s}`)},sendMessage:(e,t)=>Ft(`/api/sessions/${encodeURIComponent(e)}/message`,{method:"POST",body:JSON.stringify({message:t})}),sendMessageWithMedia:(e,t,r=[])=>{if(!Array.isArray(r)||r.length===0)return St.sendMessage(e,t);const n=new FormData;n.append("message",t);for(const s of r)n.append("files",s);return Ft(`/api/sessions/${encodeURIComponent(e)}/message`,{method:"POST",body:n})},getSessionMediaUrl:e=>{const t=new URLSearchParams({path:e});return`${Zn}/api/sessions/media?${t.toString()}`},getChannelsStatus:()=>Ft("/api/channels/status"),getConfig:()=>Ft("/api/config"),getConfigSchema:()=>Ft("/api/config/schema"),getConfigFiles:()=>Ft("/api/config/files"),saveConfig:e=>Ft("/api/config",{method:"POST",body:JSON.stringify(e)}),saveConfigFile:(e,t)=>Ft(`/api/config/files/${encodeURIComponent(e)}`,{method:"PUT",body:JSON.stringify({content:t})}),getHooks:()=>Ft("/api/hooks"),createHook:e=>Ft("/api/hooks",{method:"POST",body:JSON.stringify(e)}),updateHook:(e,t)=>Ft(`/api/hooks/${encodeURIComponent(e)}`,{method:"PUT",body:JSON.stringify(t)}),deleteHook:e=>Ft(`/api/hooks/${encodeURIComponent(e)}`,{method:"DELETE"}),toggleHook:e=>Ft(`/api/hooks/${encodeURIComponent(e)}/toggle`,{method:"PATCH"}),getMcpServers:()=>Ft("/api/mcp/servers"),getSkills:()=>Ft("/api/skills"),discoverSkills:(e="github",t="")=>{const r=new URLSearchParams;return e&&r.set("source",e),t&&r.set("query",t),Ft(`/api/skills/discover?${r.toString()}`)},installSkill:(e,t)=>Ft("/api/skills/install",{method:"POST",body:JSON.stringify({url:e,name:t})}),uninstallSkill:e=>Ft(`/api/skills/${encodeURIComponent(e)}`,{method:"DELETE"}),toggleSkill:e=>Ft(`/api/skills/${encodeURIComponent(e)}/toggle`,{method:"PATCH"}),getPlugins:()=>Ft("/api/plugins"),reloadPlugin:e=>Ft(`/api/plugins/${encodeURIComponent(e)}/reload`,{method:"POST"})},es={provider:{label:"Provider 设置",defaultOpen:!0,fields:{api_key:{type:"string",sensitive:!0,label:"API Key",desc:"当前 Provider 的 API 密钥。修改后需要重启生效",default:""},api_url:{type:"string",label:"API URL",desc:"自定义 API 端点地址。留空使用 Provider 默认值（如 Ollama 填 http://localhost:11434）",default:""},default_provider:{type:"enum",label:"默认 Provider",desc:"选择 AI 模型提供商。决定使用哪个 API 来处理请求",default:"openrouter",options:["openrouter","anthropic","openai","ollama","gemini","groq","glm","xai","compatible","copilot","claude-cli","dashscope","dashscope-coding-intl","deepseek","fireworks","mistral","together"]},default_model:{type:"string",label:"默认模型",desc:"默认使用的模型名称（如 anthropic/claude-sonnet-4-6）",default:"anthropic/claude-sonnet-4.6"},default_temperature:{type:"number",label:"温度",desc:"模型输出的随机性（0=确定性，2=最随机）。推荐日常对话 0.7，代码任务 0.3",default:.7,min:0,max:2,step:.1}}},gateway:{label:"Gateway 网关",defaultOpen:!0,fields:{"gateway.port":{type:"number",label:"端口",desc:"Gateway HTTP 服务端口号",default:3e3,min:1,max:65535},"gateway.host":{type:"string",label:"监听地址",desc:"绑定的 IP 地址。127.0.0.1 仅本机访问，0.0.0.0 允许外部访问",default:"127.0.0.1"},"gateway.require_pairing":{type:"bool",label:"需要配对",desc:"开启后必须先配对才能访问 API。关闭则任何人可直接访问（不安全）",default:!0},"gateway.allow_public_bind":{type:"bool",label:"允许公网绑定",desc:"允许绑定到非 localhost 地址而不需要隧道。通常不建议开启",default:!1},"gateway.trust_forwarded_headers":{type:"bool",label:"信任代理头",desc:"信任 X-Forwarded-For / X-Real-IP 头。仅在反向代理后方启用",default:!1},"gateway.request_timeout_secs":{type:"number",label:"请求超时(秒)",desc:"HTTP 请求处理超时时间",default:60,min:5,max:600},"gateway.pair_rate_limit_per_minute":{type:"number",label:"配对速率限制(/分)",desc:"每客户端每分钟最大配对请求数",default:10,min:1,max:100},"gateway.webhook_rate_limit_per_minute":{type:"number",label:"Webhook 速率限制(/分)",desc:"每客户端每分钟最大 Webhook 请求数",default:60,min:1,max:1e3}}},channels:{label:"消息通道",defaultOpen:!0,fields:{"channels_config.message_timeout_secs":{type:"number",label:"消息处理超时(秒)",desc:"单条消息处理的最大超时时间（LLM + 工具调用）",default:300,min:30,max:3600},"channels_config.cli":{type:"bool",label:"CLI 交互模式",desc:"启用命令行交互通道",default:!0}}},agent:{label:"Agent 编排",defaultOpen:!1,fields:{"agent.max_tool_iterations":{type:"number",label:"最大工具循环次数",desc:"每条用户消息最多执行多少轮工具调用。设 0 回退到默认 10",default:10,min:0,max:100},"agent.max_history_messages":{type:"number",label:"最大历史消息数",desc:"每个会话保留的历史消息条数",default:50,min:5,max:500},"agent.parallel_tools":{type:"bool",label:"并行工具执行",desc:"允许在单次迭代中并行调用多个工具",default:!1},"agent.compact_context":{type:"bool",label:"紧凑上下文",desc:"为小模型（13B 以下）减少上下文大小",default:!1},"agent.compaction.mode":{type:"enum",label:"上下文压缩模式",desc:"off=不压缩，safeguard=保守压缩（默认），aggressive=激进截断",default:"safeguard",options:["off","safeguard","aggressive"]},"agent.compaction.max_context_tokens":{type:"number",label:"最大上下文 Token",desc:"触发压缩的 Token 阈值",default:128e3,min:1e3,max:1e6},"agent.compaction.keep_recent_messages":{type:"number",label:"压缩后保留消息数",desc:"压缩后保留最近的非系统消息数量",default:12,min:1,max:100},"agent.compaction.memory_flush":{type:"bool",label:"压缩前刷新记忆",desc:"在压缩之前提取并保存记忆",default:!0}}},memory:{label:"记忆存储",defaultOpen:!1,fields:{"memory.backend":{type:"enum",label:"存储后端",desc:"记忆存储引擎类型",default:"sqlite",options:["sqlite","postgres","markdown","lucid","none"]},"memory.auto_save":{type:"bool",label:"自动保存",desc:"自动保存用户输入到记忆",default:!0},"memory.hygiene_enabled":{type:"bool",label:"记忆清理",desc:"定期运行记忆归档和保留清理",default:!0},"memory.archive_after_days":{type:"number",label:"归档天数",desc:"超过此天数的日志/会话文件将被归档",default:7,min:1,max:365},"memory.purge_after_days":{type:"number",label:"清除天数",desc:"归档文件超过此天数后被清除",default:30,min:1,max:3650},"memory.conversation_retention_days":{type:"number",label:"对话保留天数",desc:"SQLite 后端：超过此天数的对话记录被清理",default:3,min:1,max:365},"memory.embedding_provider":{type:"enum",label:"嵌入提供商",desc:"记忆向量化的嵌入模型提供商",default:"none",options:["none","openai","custom"]},"memory.embedding_model":{type:"string",label:"嵌入模型",desc:"嵌入模型名称（如 text-embedding-3-small）",default:"text-embedding-3-small"},"memory.embedding_dimensions":{type:"number",label:"嵌入维度",desc:"嵌入向量的维度数",default:1536,min:64,max:4096},"memory.vector_weight":{type:"number",label:"向量权重",desc:"混合搜索中向量相似度的权重（0-1）",default:.7,min:0,max:1,step:.1},"memory.keyword_weight":{type:"number",label:"关键词权重",desc:"混合搜索中 BM25 关键词匹配的权重（0-1）",default:.3,min:0,max:1,step:.1},"memory.min_relevance_score":{type:"number",label:"最低相关性分数",desc:"低于此分数的记忆不会注入上下文",default:.4,min:0,max:1,step:.05},"memory.snapshot_enabled":{type:"bool",label:"记忆快照",desc:"定期将核心记忆导出为 MEMORY_SNAPSHOT.md",default:!1},"memory.auto_hydrate":{type:"bool",label:"自动恢复",desc:"当 brain.db 不存在时自动从快照恢复",default:!0}}},security:{label:"安全策略",defaultOpen:!1,fields:{"autonomy.level":{type:"enum",label:"自主级别",desc:"read_only=只读，supervised=需审批（默认），full=完全自主",default:"supervised",options:["read_only","supervised","full"]},"autonomy.workspace_only":{type:"bool",label:"仅工作区",desc:"限制文件写入和命令执行在工作区目录内",default:!0},"autonomy.max_actions_per_hour":{type:"number",label:"每小时最大操作数",desc:"每小时允许的最大操作次数",default:20,min:1,max:1e4},"autonomy.require_approval_for_medium_risk":{type:"bool",label:"中风险需审批",desc:"中等风险的 Shell 命令需要明确批准",default:!0},"autonomy.block_high_risk_commands":{type:"bool",label:"阻止高风险命令",desc:"即使在白名单中也阻止高风险命令",default:!0},"autonomy.allowed_commands":{type:"array",label:"允许的命令",desc:"允许执行的命令白名单",default:["git","npm","cargo","ls","cat","grep","find","echo"]},"secrets.encrypt":{type:"bool",label:"加密密钥",desc:"对 config.toml 中的 API Key 和 Token 进行加密存储",default:!0}}},heartbeat:{label:"心跳检测",defaultOpen:!1,fields:{"heartbeat.enabled":{type:"bool",label:"启用心跳",desc:"启用定期心跳检查",default:!1},"heartbeat.interval_minutes":{type:"number",label:"间隔(分钟)",desc:"心跳检查的时间间隔",default:30,min:1,max:1440},"heartbeat.active_hours":{type:"array",label:"活跃时段",desc:"心跳检查的有效小时范围（如 [8, 23]）",default:[8,23]},"heartbeat.prompt":{type:"string",label:"心跳提示词",desc:"心跳触发时使用的提示词",default:"Check HEARTBEAT.md and follow instructions."}}},reliability:{label:"可靠性",defaultOpen:!1,fields:{"reliability.provider_retries":{type:"number",label:"Provider 重试次数",desc:"调用 Provider 失败后的重试次数",default:2,min:0,max:10},"reliability.provider_backoff_ms":{type:"number",label:"重试退避(ms)",desc:"Provider 重试的基础退避时间",default:500,min:100,max:3e4},"reliability.fallback_providers":{type:"array",label:"备用 Provider",desc:"主 Provider 不可用时按顺序尝试的备用列表",default:[]},"reliability.api_keys":{type:"array",label:"轮换 API Key",desc:"遇到速率限制时轮换使用的额外 API Key",default:[]},"reliability.channel_initial_backoff_secs":{type:"number",label:"通道初始退避(秒)",desc:"通道/守护进程重启的初始退避时间",default:2,min:1,max:60},"reliability.channel_max_backoff_secs":{type:"number",label:"通道最大退避(秒)",desc:"通道/守护进程重启的最大退避时间",default:60,min:5,max:3600}}},scheduler:{label:"调度器",defaultOpen:!1,fields:{"scheduler.enabled":{type:"bool",label:"启用调度器",desc:"启用内置定时任务调度循环",default:!0},"scheduler.max_tasks":{type:"number",label:"最大任务数",desc:"最多持久化保存的计划任务数量",default:64,min:1,max:1e3},"scheduler.max_concurrent":{type:"number",label:"最大并发数",desc:"每次调度周期内最多执行的任务数",default:4,min:1,max:32},"cron.enabled":{type:"bool",label:"启用 Cron",desc:"启用 Cron 子系统",default:!0},"cron.max_run_history":{type:"number",label:"Cron 历史记录数",desc:"保留的 Cron 运行历史记录条数",default:50,min:10,max:1e3}}},sessions_spawn:{label:"子进程管理",defaultOpen:!1,fields:{"sessions_spawn.default_mode":{type:"enum",label:"默认模式",desc:"子进程默认执行模式",default:"task",options:["task","process"]},"sessions_spawn.max_concurrent":{type:"number",label:"最大并发数",desc:"全局最大并发子进程/任务数",default:4,min:1,max:32},"sessions_spawn.max_spawn_depth":{type:"number",label:"最大嵌套深度",desc:"子进程可以再次 spawn 的最大深度",default:2,min:1,max:10},"sessions_spawn.max_children_per_agent":{type:"number",label:"每父进程最大子数",desc:"每个父会话允许的最大并发子运行数",default:5,min:1,max:20},"sessions_spawn.cleanup_on_complete":{type:"bool",label:"完成后清理",desc:"进程模式完成后删除工作区目录",default:!0}}},observability:{label:"可观测性",defaultOpen:!1,fields:{"observability.backend":{type:"enum",label:"后端",desc:"可观测性后端类型",default:"none",options:["none","log","prometheus","otel"]},"observability.otel_endpoint":{type:"string",label:"OTLP 端点",desc:"OpenTelemetry Collector 端点 URL（仅 otel 后端）",default:""},"observability.otel_service_name":{type:"string",label:"服务名称",desc:"上报给 OTel 的服务名称",default:"openprx"}}},web_search:{label:"网络搜索",defaultOpen:!1,fields:{"web_search.enabled":{type:"bool",label:"启用搜索",desc:"启用网络搜索工具",default:!1},"web_search.provider":{type:"enum",label:"搜索引擎",desc:"搜索提供商。DuckDuckGo 免费无 Key，Brave 需要 API Key",default:"duckduckgo",options:["duckduckgo","brave"]},"web_search.brave_api_key":{type:"string",sensitive:!0,label:"Brave API Key",desc:"Brave Search API 密钥（选 Brave 时必填）",default:""},"web_search.max_results":{type:"number",label:"最大结果数",desc:"每次搜索返回的最大结果数（1-10）",default:5,min:1,max:10},"web_search.fetch_enabled":{type:"bool",label:"启用页面抓取",desc:"允许抓取和提取网页可读内容",default:!0},"web_search.fetch_max_chars":{type:"number",label:"抓取最大字符",desc:"网页抓取返回的最大字符数",default:1e4,min:100,max:1e5}}},cost:{label:"成本控制",defaultOpen:!1,fields:{"cost.enabled":{type:"bool",label:"启用成本追踪",desc:"启用 API 调用成本追踪和预算控制",default:!1},"cost.daily_limit_usd":{type:"number",label:"日限额(USD)",desc:"每日消费上限（美元）",default:10,min:.1,max:1e4,step:.1},"cost.monthly_limit_usd":{type:"number",label:"月限额(USD)",desc:"每月消费上限（美元）",default:100,min:1,max:1e5,step:1},"cost.warn_at_percent":{type:"number",label:"预警百分比",desc:"消费达到限额的多少百分比时发出警告",default:80,min:10,max:100}}},runtime:{label:"运行时",defaultOpen:!1,fields:{"runtime.kind":{type:"enum",label:"运行时类型",desc:"命令执行环境：native=本机，docker=容器隔离",default:"native",options:["native","docker"]},"runtime.reasoning_enabled":{type:"enum",label:"推理模式",desc:"全局推理/思考模式：null=Provider 默认，true=启用，false=禁用",default:"",options:["","true","false"]}}},tunnel:{label:"隧道",defaultOpen:!1,fields:{"tunnel.provider":{type:"enum",label:"隧道类型",desc:"将 Gateway 暴露到公网的隧道服务",default:"none",options:["none","cloudflare","tailscale","ngrok","custom"]}}},identity:{label:"身份格式",defaultOpen:!1,fields:{"identity.format":{type:"enum",label:"身份格式",desc:"OpenClaw 或 AIEOS 身份文档格式",default:"openclaw",options:["openclaw","aieos"]}}}};function js(e){return String(e).replace(/_/g," ").replace(/\b\w/g,t=>t.toUpperCase())}function fu(){const e=new Set;for(const t of Object.values(es))for(const r of Object.keys(t.fields))e.add(r.split(".")[0]);return e}const vu=fu();function Cn(e){const t=Object.entries(es).map(([n,s])=>({groupKey:n,label:s.label,dynamic:!1}));if(!e||typeof e!="object")return t;const r=Object.keys(e).filter(n=>!vu.has(n)).sort().map(n=>({groupKey:n,label:js(n),dynamic:!0}));return[...t,...r]}function Hs(e){return`config-section-${e}`}function Ri(e){if(typeof document>"u"||typeof window>"u")return;const t=document.getElementById(Hs(e));t instanceof HTMLDetailsElement&&(t.open=!0),t&&t.scrollIntoView({behavior:"smooth",block:"start"});const r=`#${Hs(e)}`;window.location.hash!==r&&(window.location.hash=r)}const nr=ut({data:null,status:null,loading:!1,loaded:!1,errorMessage:""});let kn=null;function gu(e){return typeof e=="object"&&e?e:{}}async function Di({force:e=!1}={}){return kn||(nr.loaded&&!e?nr.data:(nr.loading=!0,kn=(async()=>{try{const[t,r]=await Promise.all([St.getConfig(),St.getStatus().catch(()=>null)]);return nr.data=gu(t),nr.status=r,nr.errorMessage="",nr.loaded=!0,nr.data}catch(t){throw nr.errorMessage=t instanceof Error?t.message:u("config.loadFailed"),t}finally{nr.loading=!1,kn=null}})(),kn))}function Eo(e){nr.data=e,nr.loaded=!0,nr.errorMessage=""}/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 * 
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 * 
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 * 
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 * 
 * ---
 * 
 * The MIT License (MIT) (for portions derived from Feather)
 * 
 * Copyright (c) 2013-2026 Cole Bemis
 * 
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 * 
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 * 
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 * 
 */const pu={xmlns:"http://www.w3.org/2000/svg",width:24,height:24,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor","stroke-width":2,"stroke-linecap":"round","stroke-linejoin":"round"};/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 * 
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 * 
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 * 
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 * 
 * ---
 * 
 * The MIT License (MIT) (for portions derived from Feather)
 * 
 * Copyright (c) 2013-2026 Cole Bemis
 * 
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 * 
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 * 
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 * 
 */const hu=e=>{for(const t in e)if(t.startsWith("aria-")||t==="role"||t==="title")return!0;return!1};var mu=md("<svg><!><!></svg>");function dt(e,t){pe(t,!0);const r=Wa(t,"color",3,"currentColor"),n=Wa(t,"size",3,24),s=Wa(t,"strokeWidth",3,2),i=Wa(t,"absoluteStrokeWidth",3,!1),l=Wa(t,"iconNode",19,()=>[]),d=ot(t,["$$slots","$$events","$$legacy","name","color","size","strokeWidth","absoluteStrokeWidth","iconNode","children"]);var p=mu();ko(p,(w,x)=>({...pu,...w,...d,width:n(),height:n(),stroke:r(),"stroke-width":x,class:["lucide-icon lucide",t.name&&`lucide-${t.name}`,t.class]}),[()=>!t.children&&!hu(d)&&{"aria-hidden":"true"},()=>i()?Number(s())*24/Number(n()):s()]);var f=o(p);at(f,17,l,It,(w,x)=>{var O=et(()=>Yi(a(x),2));let A=()=>a(O)[0],N=()=>a(O)[1];var S=Ie(),$=ge(S);Ed($,A,!0,(J,V)=>{ko(J,()=>({...N()}))}),m(w,S)});var y=h(f);st(y,()=>t.children??Me),m(e,p),he()}function bu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3.85 8.62a4 4 0 0 1 4.78-4.77 4 4 0 0 1 6.74 0 4 4 0 0 1 4.78 4.78 4 4 0 0 1 0 6.74 4 4 0 0 1-4.77 4.78 4 4 0 0 1-6.75 0 4 4 0 0 1-4.78-4.77 4 4 0 0 1 0-6.76Z"}],["path",{d:"m9 12 2 2 4-4"}]];dt(e,lt({name:"badge-check"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Po(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M10 22V7a1 1 0 0 0-1-1H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-5a1 1 0 0 0-1-1H2"}],["rect",{x:"14",y:"2",width:"8",height:"8",rx:"1"}]];dt(e,lt({name:"blocks"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function yu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 8V4H8"}],["rect",{width:"16",height:"12",x:"4",y:"8",rx:"2"}],["path",{d:"M2 14h2"}],["path",{d:"M20 14h2"}],["path",{d:"M15 13v2"}],["path",{d:"M9 13v2"}]];dt(e,lt({name:"bot"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function _u(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 18V5"}],["path",{d:"M15 13a4.17 4.17 0 0 1-3-4 4.17 4.17 0 0 1-3 4"}],["path",{d:"M17.598 6.5A3 3 0 1 0 12 5a3 3 0 1 0-5.598 1.5"}],["path",{d:"M17.997 5.125a4 4 0 0 1 2.526 5.77"}],["path",{d:"M18 18a4 4 0 0 0 2-7.464"}],["path",{d:"M19.967 17.483A4 4 0 1 1 12 18a4 4 0 1 1-7.967-.517"}],["path",{d:"M6 18a4 4 0 0 1-2-7.464"}],["path",{d:"M6.003 5.125a4 4 0 0 0-2.526 5.77"}]];dt(e,lt({name:"brain"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function xu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M17 19a1 1 0 0 1-1-1v-2a2 2 0 0 1 2-2h2a2 2 0 0 1 2 2v2a1 1 0 0 1-1 1z"}],["path",{d:"M17 21v-2"}],["path",{d:"M19 14V6.5a1 1 0 0 0-7 0v11a1 1 0 0 1-7 0V10"}],["path",{d:"M21 21v-2"}],["path",{d:"M3 5V3"}],["path",{d:"M4 10a2 2 0 0 1-2-2V6a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2a2 2 0 0 1-2 2z"}],["path",{d:"M7 5V3"}]];dt(e,lt({name:"cable"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function ku(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3 3v16a2 2 0 0 0 2 2h16"}],["path",{d:"M18 17V9"}],["path",{d:"M13 17V5"}],["path",{d:"M8 17v-3"}]];dt(e,lt({name:"chart-column"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function To(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m6 9 6 6 6-6"}]];dt(e,lt({name:"chevron-down"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function wu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["line",{x1:"12",x2:"12",y1:"8",y2:"12"}],["line",{x1:"12",x2:"12.01",y1:"16",y2:"16"}]];dt(e,lt({name:"circle-alert"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Su(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M21.801 10A10 10 0 1 1 17 3.335"}],["path",{d:"m9 11 3 3L22 4"}]];dt(e,lt({name:"circle-check-big"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function $u(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["path",{d:"M12 6v6l4 2"}]];dt(e,lt({name:"clock"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Mu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["rect",{width:"14",height:"14",x:"8",y:"8",rx:"2",ry:"2"}],["path",{d:"M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"}]];dt(e,lt({name:"copy"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Cu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["ellipse",{cx:"12",cy:"5",rx:"9",ry:"3"}],["path",{d:"M3 5V19A9 3 0 0 0 21 19V5"}],["path",{d:"M3 12A9 3 0 0 0 21 12"}]];dt(e,lt({name:"database"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Au(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["line",{x1:"12",x2:"12",y1:"2",y2:"22"}],["path",{d:"M17 5H9.5a3.5 3.5 0 0 0 0 7h5a3.5 3.5 0 0 1 0 7H6"}]];dt(e,lt({name:"dollar-sign"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Eu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M10.733 5.076a10.744 10.744 0 0 1 11.205 6.575 1 1 0 0 1 0 .696 10.747 10.747 0 0 1-1.444 2.49"}],["path",{d:"M14.084 14.158a3 3 0 0 1-4.242-4.242"}],["path",{d:"M17.479 17.499a10.75 10.75 0 0 1-15.417-5.151 1 1 0 0 1 0-.696 10.75 10.75 0 0 1 4.446-5.143"}],["path",{d:"m2 2 20 20"}]];dt(e,lt({name:"eye-off"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Pu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0"}],["circle",{cx:"12",cy:"12",r:"3"}]];dt(e,lt({name:"eye"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Tu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M14 22h4a2 2 0 0 0 2-2V8a2.4 2.4 0 0 0-.706-1.706l-3.588-3.588A2.4 2.4 0 0 0 14 2H6a2 2 0 0 0-2 2v6"}],["path",{d:"M14 2v5a1 1 0 0 0 1 1h5"}],["path",{d:"M5 14a1 1 0 0 0-1 1v2a1 1 0 0 1-1 1 1 1 0 0 1 1 1v2a1 1 0 0 0 1 1"}],["path",{d:"M9 22a1 1 0 0 0 1-1v-2a1 1 0 0 1 1-1 1 1 0 0 1-1-1v-2a1 1 0 0 0-1-1"}]];dt(e,lt({name:"file-braces-corner"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Fu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M15 2h-4a2 2 0 0 0-2 2v11a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2V8"}],["path",{d:"M16.706 2.706A2.4 2.4 0 0 0 15 2v5a1 1 0 0 0 1 1h5a2.4 2.4 0 0 0-.706-1.706z"}],["path",{d:"M5 7a2 2 0 0 0-2 2v11a2 2 0 0 0 2 2h8a2 2 0 0 0 1.732-1"}]];dt(e,lt({name:"files"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Nu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M15 6a9 9 0 0 0-9 9V3"}],["circle",{cx:"18",cy:"6",r:"3"}],["circle",{cx:"6",cy:"18",r:"3"}]];dt(e,lt({name:"git-branch"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Ou(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["path",{d:"M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"}],["path",{d:"M2 12h20"}]];dt(e,lt({name:"globe"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Lu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M2 9.5a5.5 5.5 0 0 1 9.591-3.676.56.56 0 0 0 .818 0A5.49 5.49 0 0 1 22 9.5c0 2.29-1.5 4-3 5.5l-5.492 5.313a2 2 0 0 1-3 .019L5 15c-1.5-1.5-3-3.2-3-5.5"}],["path",{d:"M3.22 13H9.5l.5-1 2 4.5 2-7 1.5 3.5h5.27"}]];dt(e,lt({name:"heart-pulse"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Iu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 2v4"}],["path",{d:"m16.2 7.8 2.9-2.9"}],["path",{d:"M18 12h4"}],["path",{d:"m16.2 16.2 2.9 2.9"}],["path",{d:"M12 18v4"}],["path",{d:"m4.9 19.1 2.9-2.9"}],["path",{d:"M2 12h4"}],["path",{d:"m4.9 4.9 2.9 2.9"}]];dt(e,lt({name:"loader"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Ru(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z"}]];dt(e,lt({name:"message-square"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Du(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401"}]];dt(e,lt({name:"moon"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function ju(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m16 6-8.414 8.586a2 2 0 0 0 2.829 2.829l8.414-8.586a4 4 0 1 0-5.657-5.657l-8.379 8.551a6 6 0 1 0 8.485 8.485l8.379-8.551"}]];dt(e,lt({name:"paperclip"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Us(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"}],["path",{d:"M21 3v5h-5"}],["path",{d:"M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"}],["path",{d:"M8 16H3v5"}]];dt(e,lt({name:"refresh-cw"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Fo(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"}],["path",{d:"M3 3v5h5"}]];dt(e,lt({name:"rotate-ccw"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function _s(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M15.2 3a2 2 0 0 1 1.4.6l3.8 3.8a2 2 0 0 1 .6 1.4V19a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z"}],["path",{d:"M17 21v-7a1 1 0 0 0-1-1H8a1 1 0 0 0-1 1v7"}],["path",{d:"M7 3v4a1 1 0 0 0 1 1h7"}]];dt(e,lt({name:"save"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function No(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m21 21-4.34-4.34"}],["circle",{cx:"11",cy:"11",r:"8"}]];dt(e,lt({name:"search"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Hu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915"}],["circle",{cx:"12",cy:"12",r:"3"}]];dt(e,lt({name:"settings"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Uu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"}]];dt(e,lt({name:"shield"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function zu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"4"}],["path",{d:"M12 2v2"}],["path",{d:"M12 20v2"}],["path",{d:"m4.93 4.93 1.41 1.41"}],["path",{d:"m17.66 17.66 1.41 1.41"}],["path",{d:"M2 12h2"}],["path",{d:"M20 12h2"}],["path",{d:"m6.34 17.66-1.41 1.41"}],["path",{d:"m19.07 4.93-1.41 1.41"}]];dt(e,lt({name:"sun"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}function Vu(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=ot(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M4 14a1 1 0 0 1-.78-1.63l9.9-10.2a.5.5 0 0 1 .86.46l-1.92 6.02A1 1 0 0 0 13 10h7a1 1 0 0 1 .78 1.63l-9.9 10.2a.5.5 0 0 1-.86-.46l1.92-6.02A1 1 0 0 0 11 14z"}]];dt(e,lt({name:"zap"},()=>r,{get iconNode(){return n},children:(s,i)=>{var l=Ie(),d=ge(l);st(d,()=>t.children??Me),m(s,l)},$$slots:{default:!0}})),he()}var qu=k("<option> </option>"),Bu=k('<p class="text-sm text-red-500 dark:text-red-400"> </p>'),Ku=k('<div class="flex min-h-screen items-center justify-center bg-gray-50 px-4 py-8 text-gray-900 dark:bg-gray-900 dark:text-gray-100"><div class="w-full max-w-md rounded-xl border border-gray-200 bg-white p-6 shadow-xl shadow-black/10 dark:border-gray-700 dark:bg-gray-800 dark:shadow-black/30"><div class="flex items-center justify-between gap-3"><h1 class="text-2xl font-semibold tracking-tight"> </h1> <label class="sr-only" for="login-language-select"> </label> <select id="login-language-select" class="rounded-lg border border-gray-300 bg-gray-50 px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 dark:hover:bg-gray-700"></select></div> <p class="mt-2 text-sm text-gray-500 dark:text-gray-400"> </p> <form class="mt-6 space-y-4"><label class="block text-sm font-medium text-gray-600 dark:text-gray-300" for="token"> </label> <input id="token" type="password" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-gray-900 outline-none ring-sky-500 transition focus:ring-2 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100" autocomplete="off"/> <!> <button type="submit" class="w-full rounded-lg bg-sky-600 px-4 py-2 font-medium text-white transition hover:bg-sky-500"> </button></form></div></div>');function Wu(e,t){pe(t,!0);let r=L(""),n=L("");const s=et(()=>no.map(B=>({value:B,label:u(`languages.${B}`)})));function i(B){var oe;B.preventDefault();const X=a(r).trim();if(!X){c(n,u("login.tokenRequired"),!0);return}Gd(X),c(n,""),(oe=t.onLogin)==null||oe.call(t,X)}var l=Ku(),d=o(l),p=o(d),f=o(p),y=o(f),w=h(f,2),x=o(w),O=h(w,2);at(O,21,()=>a(s),It,(B,X)=>{var oe=qu(),ye=o(oe),_e={};C(()=>{v(ye,a(X).label),_e!==(_e=a(X).value)&&(oe.value=(oe.__value=a(X).value)??"")}),m(B,oe)});var A=h(p,2),N=o(A),S=h(A,2),$=o(S),J=o($),V=h($,2),E=h(V,2);{var M=B=>{var X=Bu(),oe=o(X);C(()=>v(oe,a(n))),m(B,X)};W(E,B=>{a(n)&&B(M)})}var R=h(E,2),D=o(R);C((B,X,oe,ye,_e,G,ne)=>{v(y,B),v(x,X),ve(O,"aria-label",oe),v(N,ye),v(J,_e),ve(V,"placeholder",G),v(D,ne)},[()=>u("login.title"),()=>u("app.language"),()=>u("app.language"),()=>u("login.hint"),()=>u("login.accessToken"),()=>u("login.placeholder"),()=>u("login.login")]),re("change",O,B=>so(B.currentTarget.value)),Ua(O,()=>$a.lang,B=>$a.lang=B),va("submit",S,i),Zr(V,()=>a(r),B=>c(r,B)),m(e,l),he()}Ur(["change"]);function Ju(e){if(!Number.isFinite(e)||e<0)return`0${u("common.durationUnits.second")}`;const t=Math.floor(e/86400),r=Math.floor(e%86400/3600),n=Math.floor(e%3600/60),s=Math.floor(e%60),i=[];return t>0&&i.push(`${t}${u("common.durationUnits.day")}`),(r>0||i.length>0)&&i.push(`${r}${u("common.durationUnits.hour")}`),(n>0||i.length>0)&&i.push(`${n}${u("common.durationUnits.minute")}`),i.push(`${s}${u("common.durationUnits.second")}`),i.join(" ")}var Gu=k('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),Qu=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Yu=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Xu=k('<div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><p class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400"> </p> <p class="mt-2 text-lg font-semibold text-gray-900 dark:text-gray-100"> </p></div>'),Zu=k('<p class="mt-3 text-sm text-gray-500 dark:text-gray-400"> </p>'),ef=k('<li class="rounded-full border border-gray-300 bg-gray-50 px-3 py-1 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"> </li>'),tf=k('<ul class="mt-3 flex flex-wrap gap-2"></ul>'),rf=k('<div class="grid gap-4 sm:grid-cols-2 xl:grid-cols-5"></div> <div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><h3 class="text-sm font-semibold uppercase tracking-wide text-gray-600 dark:text-gray-300"> </h3> <!></div>',1),af=k('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function nf(e,t){pe(t,!0);let r=L(null),n=L(!0),s=L(""),i=L("");function l(M){return typeof M!="string"||M.length===0?u("common.unknown"):M.replaceAll("_"," ").split(" ").map(R=>R.charAt(0).toUpperCase()+R.slice(1)).join(" ")}function d(M){const R=`channels.names.${M}`,D=u(R);return D===R?l(M):D}const p=et(()=>{var M,R,D,B,X;return[{label:u("overview.version"),value:((M=a(r))==null?void 0:M.version)??u("common.na")},{label:u("overview.uptime"),value:typeof((R=a(r))==null?void 0:R.uptime_seconds)=="number"?Ju(a(r).uptime_seconds):u("common.na")},{label:u("overview.model"),value:((D=a(r))==null?void 0:D.model)??u("common.na")},{label:u("overview.memoryBackend"),value:((B=a(r))==null?void 0:B.memory_backend)??u("common.na")},{label:u("overview.gatewayPort"),value:(X=a(r))!=null&&X.gateway_port?String(a(r).gateway_port):u("common.na")}]}),f=et(()=>{var M;return Array.isArray((M=a(r))==null?void 0:M.channels)?a(r).channels:[]});async function y(){try{const M=await St.getStatus();c(r,M,!0),c(s,""),c(i,new Date().toLocaleTimeString(),!0)}catch(M){c(s,M instanceof Error?M.message:u("overview.loadFailed"),!0)}finally{c(n,!1)}}lr(()=>{let M=!1;const R=async()=>{M||await y()};R();const D=setInterval(R,3e4);return()=>{M=!0,clearInterval(D)}});var w=af(),x=o(w),O=o(x),A=o(O),N=h(O,2);{var S=M=>{var R=Gu(),D=o(R);C(B=>v(D,B),[()=>u("common.updatedAt",{time:a(i)})]),m(M,R)};W(N,M=>{a(i)&&M(S)})}var $=h(x,2);{var J=M=>{var R=Qu(),D=o(R);C(B=>v(D,B),[()=>u("overview.loading")]),m(M,R)},V=M=>{var R=Yu(),D=o(R);C(()=>v(D,a(s))),m(M,R)},E=M=>{var R=rf(),D=ge(R);at(D,21,()=>a(p),It,(ne,ce)=>{var de=Xu(),ze=o(de),Ce=o(ze),Y=h(ze,2),Qe=o(Y);C(()=>{v(Ce,a(ce).label),v(Qe,a(ce).value)}),m(ne,de)});var B=h(D,2),X=o(B),oe=o(X),ye=h(X,2);{var _e=ne=>{var ce=Zu(),de=o(ce);C(ze=>v(de,ze),[()=>u("overview.noChannelsConfigured")]),m(ne,ce)},G=ne=>{var ce=tf();at(ce,21,()=>a(f),It,(de,ze)=>{var Ce=ef(),Y=o(Ce);C(Qe=>v(Y,Qe),[()=>d(a(ze))]),m(de,Ce)}),m(ne,ce)};W(ye,ne=>{a(f).length===0?ne(_e):ne(G,-1)})}C(ne=>v(oe,ne),[()=>u("overview.configuredChannels")]),m(M,R)};W($,M=>{a(n)?M(J):a(s)?M(V,1):M(E,-1)})}C(M=>v(A,M),[()=>u("overview.title")]),m(e,w),he()}var sf=k('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),of=k('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),lf=k("<option> </option>"),df=k("<option> </option>"),cf=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),uf=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),ff=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),vf=k('<tr class="cursor-pointer transition hover:bg-gray-50 dark:hover:bg-gray-700/40"><td class="px-4 py-3 font-mono text-xs"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"><span class="rounded-full border border-gray-300/70 px-2 py-1 text-xs dark:border-gray-600/70"> </span></td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td></tr>'),gf=k('<div class="overflow-x-auto rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><table class="min-w-full divide-y divide-gray-200 text-sm dark:divide-gray-700"><thead class="bg-gray-50 text-left text-gray-600 dark:bg-gray-900/50 dark:text-gray-300"><tr><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th></tr></thead><tbody class="divide-y divide-gray-200 text-gray-700 dark:divide-gray-700 dark:text-gray-200"></tbody></table></div> <div class="flex items-center justify-between gap-3"><p class="text-sm text-gray-500 dark:text-gray-400"> </p> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></div>',1),pf=k('<section class="space-y-6"><div class="flex flex-wrap items-center justify-between gap-3"><div class="flex items-center gap-3"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></div> <div class="grid gap-3 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800 lg:grid-cols-[minmax(0,1.3fr)_220px_220px_auto]"><input type="search" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <select class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"><option> </option><!></select> <select class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select> <button type="button" class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500"> </button></div> <!></section>');function hf(e,t){pe(t,!0);const r=20,n=["all","active","pending","empty"];let s=L(ut([])),i=L(!0),l=L(!1),d=L(""),p=L(""),f=L(""),y=L("all"),w=L(""),x=L(0),O=L(!1);function A(T){return typeof T!="string"||T.length===0?u("common.unknown"):T.replaceAll("_"," ").split(" ").map(U=>U.charAt(0).toUpperCase()+U.slice(1)).join(" ")}function N(T){const U=`channels.names.${T}`,K=u(U);return K===U?A(T):K}function S(T){const U=`sessions.status.${T}`,K=u(U);return K===U?A(T):K}const $=et(()=>[...new Set(a(s).map(T=>T.channel).filter(Boolean))].sort((T,U)=>T.localeCompare(U)));async function J({reset:T=!1,targetPage:U}={}){const K=typeof U=="number"?U:T?0:a(x);c(T?i:l,!0);try{const j=await St.getSessions({limit:r+1,offset:K*r,channel:a(f)||void 0,status:a(y)==="all"?void 0:a(y),search:a(w).trim()||void 0}),Z=Array.isArray(j)?j:[];c(O,Z.length>r),c(s,a(O)?Z.slice(0,r):Z,!0),c(d,""),c(p,new Date().toLocaleTimeString(),!0),c(x,K,!0)}catch(j){c(d,j instanceof Error?j.message:u("sessions.loadFailed"),!0),T&&c(s,[],!0)}finally{c(i,!1),c(l,!1)}}function V(T){ga(`/chat/${encodeURIComponent(T)}`)}function E(){J({reset:!0})}function M(){a(x)!==0&&J({targetPage:a(x)-1})}function R(){a(O)&&J({targetPage:a(x)+1})}lr(()=>{let T=!1;const U=async()=>{T||await J({reset:!0})};U();const K=setInterval(U,15e3);return()=>{T=!0,clearInterval(K)}});var D=pf(),B=o(D),X=o(B),oe=o(X),ye=o(oe),_e=h(oe,2);{var G=T=>{var U=sf(),K=o(U);C(j=>v(K,j),[()=>u("common.loading")]),m(T,U)};W(_e,T=>{a(l)&&!a(i)&&T(G)})}var ne=h(X,2);{var ce=T=>{var U=of(),K=o(U);C(j=>v(K,j),[()=>u("common.updatedAt",{time:a(p)})]),m(T,U)};W(ne,T=>{a(p)&&T(ce)})}var de=h(B,2),ze=o(de),Ce=h(ze,2),Y=o(Ce),Qe=o(Y);Y.value=Y.__value="";var Mt=h(Y);at(Mt,17,()=>a($),It,(T,U)=>{var K=lf(),j=o(K),Z={};C(Ne=>{v(j,Ne),Z!==(Z=a(U))&&(K.value=(K.__value=a(U))??"")},[()=>N(a(U))]),m(T,K)});var ae=h(Ce,2);at(ae,21,()=>n,It,(T,U)=>{var K=df(),j=o(K),Z={};C(Ne=>{v(j,Ne),Z!==(Z=a(U))&&(K.value=(K.__value=a(U))??"")},[()=>S(a(U))]),m(T,K)});var xe=h(ae,2),Fe=o(xe),ft=h(de,2);{var jt=T=>{var U=cf(),K=o(U);C(j=>v(K,j),[()=>u("sessions.loading")]),m(T,U)},Nt=T=>{var U=uf(),K=o(U);C(()=>v(K,a(d))),m(T,U)},At=T=>{var U=ff(),K=o(U);C(j=>v(K,j),[()=>u("sessions.none")]),m(T,U)},Vt=T=>{var U=gf(),K=ge(U),j=o(K),Z=o(j),Ne=o(Z),Oe=o(Ne),Re=o(Oe),Ke=h(Oe),we=o(Ke),Ve=h(Ke),Ot=o(Ve),le=h(Ve),De=o(le),ue=h(le),ke=o(ue),je=h(ue),se=o(je),Et=h(Z);at(Et,21,()=>a(s),It,(Dt,Le)=>{var z=vf(),me=o(z),We=o(me),qe=h(me),_=o(qe),H=h(qe),q=o(H),Q=h(H),Ye=o(Q),Se=o(Ye),Ae=h(Q),Tt=o(Ae),gt=h(Ae),pt=o(gt);C((ie,Ee,rt)=>{v(We,a(Le).session_id),v(_,a(Le).sender),v(q,ie),v(Se,Ee),v(Tt,a(Le).message_count),v(pt,rt)},[()=>N(a(Le).channel),()=>S(a(Le).status),()=>a(Le).last_message_preview||u("common.empty")]),re("click",z,()=>V(a(Le).session_id)),m(Dt,z)});var vt=h(K,2),ht=o(vt),Ct=o(ht),Ht=h(ht,2),Pt=o(Ht),Rt=o(Pt),Ut=h(Pt,2),Kt=o(Ut);C((Dt,Le,z,me,We,qe,_,H,q)=>{v(Re,Dt),v(we,Le),v(Ot,z),v(De,me),v(ke,We),v(se,qe),v(Ct,_),Pt.disabled=a(x)===0,v(Rt,H),Ut.disabled=!a(O),v(Kt,q)},[()=>u("sessions.sessionId"),()=>u("sessions.sender"),()=>u("sessions.channel"),()=>u("sessions.statusLabel"),()=>u("sessions.messages"),()=>u("sessions.lastMessage"),()=>u("sessions.pageLabel",{page:a(x)+1}),()=>u("sessions.previousPage"),()=>u("sessions.nextPage")]),re("click",Pt,M),re("click",Ut,R),m(T,U)};W(ft,T=>{a(i)?T(jt):a(d)?T(Nt,1):a(s).length===0?T(At,2):T(Vt,-1)})}C((T,U,K,j)=>{v(ye,T),ve(ze,"placeholder",U),v(Qe,K),v(Fe,j)},[()=>u("sessions.title"),()=>u("sessions.searchPlaceholder"),()=>u("sessions.allChannels"),()=>u("sessions.applyFilters")]),re("keydown",ze,T=>{T.key==="Enter"&&E()}),Zr(ze,()=>a(w),T=>c(w,T)),Ua(Ce,()=>a(f),T=>c(f,T)),Ua(ae,()=>a(y),T=>c(y,T)),re("click",xe,E),m(e,D),he()}Ur(["keydown","click"]);var mf=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),bf=k('<p class="mb-3 rounded-lg border border-blue-500/40 bg-blue-500/15 px-3 py-2 text-sm text-blue-700 dark:text-blue-200"> </p>'),yf=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),_f=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),xf=k('<div class="flex justify-center"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div>'),kf=k('<p class="whitespace-pre-wrap break-words text-sm"> </p>'),wf=k('<img class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 object-contain dark:border-gray-600/40" loading="lazy"/>'),Sf=k('<video controls="" class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 dark:border-gray-600/40"></video>',2),$f=k("<div></div>"),Mf=k('<div class="space-y-3"><!> <!></div>'),Cf=k('<img class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"/>'),Af=k('<video class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"></video>',2),Ef=k('<div class="flex h-12 w-12 items-center justify-center rounded border border-gray-300 bg-gray-100 text-lg text-gray-700 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200"> </div>'),Pf=k('<div class="flex items-center gap-2 rounded-md border border-gray-200 bg-white/90 p-2 dark:border-gray-700 dark:bg-gray-800/90"><!> <div class="min-w-0 flex-1"><p class="truncate text-sm text-gray-900 dark:text-gray-100"> </p> <p class="truncate text-xs text-gray-500 dark:text-gray-400"> </p></div> <button type="button" class="rounded px-2 py-1 text-xs text-gray-600 hover:bg-gray-200 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-white"> </button></div>'),Tf=k('<div class="mb-3 space-y-2 rounded-lg border border-gray-200 bg-gray-50/70 p-2.5 dark:border-gray-700 dark:bg-gray-900/70"><p class="text-xs text-gray-600 dark:text-gray-300"> </p> <div class="max-h-44 space-y-2 overflow-y-auto pr-1"></div></div>'),Ff=k('<section class="flex h-[calc(100vh-10rem)] flex-col gap-4"><div class="flex items-center justify-between"><div class="min-w-0"><h2 class="text-2xl font-semibold"> </h2> <p class="truncate font-mono text-xs text-gray-500 dark:text-gray-400"> </p></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!> <div class="flex min-h-0 flex-1 flex-col rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800" role="region"><div><!> <!></div> <form class="border-t border-gray-200 p-3 dark:border-gray-700"><input type="file" class="hidden" multiple="" accept="image/*,video/*,.pdf,.doc,.docx,.txt,.md,.csv,.json,.zip,.tar,.gz,.rar,.ppt,.pptx,.xls,.xlsx"/> <!> <div class="flex items-end gap-2"><textarea rows="2" class="min-h-[2.75rem] flex-1 resize-y rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-900 outline-none focus:border-blue-500 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100"></textarea> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-600 hover:border-gray-400 hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:border-gray-500 dark:hover:bg-gray-700"><!></button> <button type="submit" class="rounded-lg bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"> </button></div></form></div></section>');function Nf(e,t){pe(t,!0);const r=10,n=40,s=80,i=/\[(IMAGE|VIDEO):([^\]]+)\]|(data:(?:image|video)\/[a-zA-Z0-9.+-]+;base64,[a-zA-Z0-9+/=]+)/gi;let l=Wa(t,"sessionId",3,""),d=L(ut([])),p=L(""),f=L(!0),y=L(!1),w=L(!1),x=L(!1),O=L(0),A=L(""),N=L(null),S=L(null),$=L(ut([])),J=L(!1),V=0;function E(_){return _!=null&&_.message_id?`id:${_.message_id}`:`fallback:${(_==null?void 0:_.timestamp)??""}:${(_==null?void 0:_.role)??""}:${(_==null?void 0:_.content)??""}`}function M(_){const H=new Set,q=[];for(const Q of _){const Ye=E(Q);H.has(Ye)||(H.add(Ye),q.push(Q))}return q}function R(){ga("/sessions")}function D(_){return _==="user"?"ml-auto max-w-[85%] rounded-2xl rounded-br-md bg-blue-600 px-4 py-2 text-white":_==="assistant"?"mr-auto max-w-[85%] rounded-2xl rounded-bl-md bg-gray-200 px-4 py-2 text-gray-900 dark:bg-gray-700 dark:text-gray-100":"mx-auto max-w-[90%] rounded-lg bg-gray-100/60 px-3 py-1.5 text-center text-xs text-gray-500 dark:bg-gray-800/60 dark:text-gray-400"}function B(_){return((_==null?void 0:_.type)||"").startsWith("image/")}function X(_){return((_==null?void 0:_.type)||"").startsWith("video/")}function oe(_){if(!Number.isFinite(_)||_<=0)return`0 ${u("common.fileSizeUnits.b")}`;const H=["b","kb","mb","gb"];let q=_,Q=0;for(;q>=1024&&Q<H.length-1;)q/=1024,Q+=1;return`${q.toFixed(Q===0?0:1)} ${u(`common.fileSizeUnits.${H[Q]}`)}`}function ye(_){return typeof _=="string"&&_.trim().length>0?_:u("common.fileTypeUnknown")}function _e(_){const H=B(_),q=X(_);return{id:`${_.name}-${_.lastModified}-${Math.random().toString(36).slice(2)}`,file:_,name:_.name,size:_.size,type:ye(_.type),isImage:H,isVideo:q,previewUrl:H||q?URL.createObjectURL(_):""}}function G(_){_&&typeof _.previewUrl=="string"&&_.previewUrl.startsWith("blob:")&&URL.revokeObjectURL(_.previewUrl)}function ne(){for(const _ of a($))G(_);c($,[],!0),a(S)&&(a(S).value="")}function ce(_){if(!_||_.length===0||a(y))return;const H=Array.from(_),q=[],Q=Math.max(0,r-a($).length);for(const Ye of H.slice(0,Q))q.push(_e(Ye));c($,[...a($),...q],!0)}function de(_){const H=a($).find(q=>q.id===_);H&&G(H),c($,a($).filter(q=>q.id!==_),!0)}function ze(){var _;a(y)||(_=a(S))==null||_.click()}function Ce(_){var H;ce((H=_.currentTarget)==null?void 0:H.files),a(S)&&(a(S).value="")}function Y(_){_.preventDefault(),!a(y)&&(V+=1,c(J,!0))}function Qe(_){_.preventDefault(),!a(y)&&_.dataTransfer&&(_.dataTransfer.dropEffect="copy")}function Mt(_){_.preventDefault(),V=Math.max(0,V-1),V===0&&c(J,!1)}function ae(_){var H;_.preventDefault(),V=0,c(J,!1),ce((H=_.dataTransfer)==null?void 0:H.files)}function xe(){!a(N)||a(f)||a(y)||a(w)||!a(x)||a(N).scrollTop<=s&&T()}function Fe(_){const H=(_||"").trim();if(!H)return"";const q=H.toLowerCase();return q.startsWith("data:image/")||q.startsWith("data:video/")||q.startsWith("http://")||q.startsWith("https://")?H:St.getSessionMediaUrl(H)}function ft(_,H){const q=(H||"").trim().toLowerCase();return _==="VIDEO"||q.startsWith("data:video/")?"video":q.startsWith("data:image/")?"image":[".mp4",".webm",".mov",".m4v",".ogg"].some(Ye=>q.endsWith(Ye))?"video":"image"}function jt(_){if(typeof _!="string"||_.length===0)return[];const H=[];i.lastIndex=0;let q=0,Q;for(;(Q=i.exec(_))!==null;){Q.index>q&&H.push({id:`text-${q}`,kind:"text",value:_.slice(q,Q.index)});const Ye=(Q[1]||"").toUpperCase(),Se=(Q[2]||Q[3]||"").trim();if(Se){const Ae=ft(Ye,Se);H.push({id:`${Ae}-${Q.index}`,kind:Ae,value:Se})}q=i.lastIndex}return q<_.length&&H.push({id:`text-tail-${q}`,kind:"text",value:_.slice(q)}),H}async function Nt(){await Fs(),a(N)&&(a(N).scrollTop=a(N).scrollHeight)}async function At(_,{appendOlder:H=!1}={}){const q=await St.getSessionMessages(l(),{limit:n+1,offset:_}),Q=Array.isArray(q)?q:[],Ye=Q.length>n,Se=Ye?Q.slice(0,n):Q;if(H&&a(N)){const Ae=a(N).scrollHeight;c(d,M([...Se,...a(d)]),!0),c(O,a(d).length,!0),c(x,Ye),await Fs(),a(N).scrollTop=a(N).scrollHeight-Ae;return}c(d,M(Se),!0),c(O,a(d).length,!0),c(x,Ye)}async function Vt(){try{await At(0),c(A,""),await Nt()}catch(_){c(A,_ instanceof Error?_.message:u("chat.loadFailed"),!0)}finally{c(f,!1)}}async function T(){if(!(a(w)||!a(x))){c(w,!0);try{await At(a(O),{appendOlder:!0}),c(A,"")}catch(_){c(A,_ instanceof Error?_.message:u("chat.loadFailed"),!0)}finally{c(w,!1)}}}async function U(){try{await At(0),c(A,""),await Nt()}catch(_){c(A,_ instanceof Error?_.message:u("chat.loadFailed"),!0)}}async function K(){const _=a(p).trim(),H=a($).map(Q=>Q.file);if(_.length===0&&H.length===0||a(y))return;c(y,!0),c(p,""),c(A,"");const q=H.length>0;q||(c(d,[...a(d),{role:"user",content:_}],!0),await Nt());try{const Q=q?await St.sendMessageWithMedia(l(),_,H):await St.sendMessage(l(),_);q?await U():Q&&typeof Q.reply=="string"&&Q.reply.length>0&&c(d,[...a(d),{role:"assistant",content:Q.reply}],!0),ne()}catch(Q){c(A,Q instanceof Error?Q.message:u("chat.sendFailed"),!0),await U()}finally{c(y,!1),await Nt()}}function j(_){_.preventDefault(),K()}lr(()=>{let _=!1;return(async()=>{_||(c(f,!0),await Vt())})(),()=>{_=!0}}),Bd(()=>{for(const _ of a($))G(_)});var Z=Ff(),Ne=o(Z),Oe=o(Ne),Re=o(Oe),Ke=o(Re),we=h(Re,2),Ve=o(we),Ot=h(Oe,2),le=o(Ot),De=h(Ne,2);{var ue=_=>{var H=mf(),q=o(H);C(()=>v(q,a(A))),m(_,H)};W(De,_=>{a(A)&&_(ue)})}var ke=h(De,2),je=o(ke),se=o(je);{var Et=_=>{var H=bf(),q=o(H);C(Q=>v(q,Q),[()=>u("chat.dropFiles",{count:a($).length,max:r})]),m(_,H)};W(se,_=>{a(J)&&_(Et)})}var vt=h(se,2);{var ht=_=>{var H=yf(),q=o(H);C(Q=>v(q,Q),[()=>u("chat.loading")]),m(_,H)},Ct=_=>{var H=_f(),q=o(H);C(Q=>v(q,Q),[()=>u("chat.empty")]),m(_,H)},Ht=_=>{var H=Mf(),q=o(H);{var Q=Se=>{var Ae=xf(),Tt=o(Ae),gt=o(Tt);C(pt=>{Tt.disabled=a(w),v(gt,pt)},[()=>a(w)?u("chat.loadingMore"):u("chat.loadMore")]),re("click",Tt,T),m(Se,Ae)};W(q,Se=>{a(x)&&Se(Q)})}var Ye=h(q,2);at(Ye,19,()=>a(d),(Se,Ae)=>Se.message_id??`${Se.timestamp??"local"}-${Ae}`,(Se,Ae)=>{var Tt=$f();at(Tt,21,()=>jt(a(Ae).content),gt=>gt.id,(gt,pt)=>{var ie=Ie(),Ee=ge(ie);{var rt=qt=>{var Xt=Ie(),Mr=ge(Xt);{var zr=Bt=>{var da=kf(),Ca=o(da);C(()=>v(Ca,a(pt).value)),m(Bt,da)},tr=et(()=>a(pt).value.trim().length>0);W(Mr,Bt=>{a(tr)&&Bt(zr)})}m(qt,Xt)},er=qt=>{var Xt=wf();C((Mr,zr)=>{ve(Xt,"src",Mr),ve(Xt,"alt",zr)},[()=>Fe(a(pt).value),()=>u("chat.attachmentAlt")]),m(qt,Xt)},$r=qt=>{var Xt=Sf();C(Mr=>ve(Xt,"src",Mr),[()=>Fe(a(pt).value)]),m(qt,Xt)};W(Ee,qt=>{a(pt).kind==="text"?qt(rt):a(pt).kind==="image"?qt(er,1):a(pt).kind==="video"&&qt($r,2)})}m(gt,ie)}),C(gt=>_t(Tt,1,gt),[()=>Zs(D(a(Ae).role))]),m(Se,Tt)}),m(_,H)};W(vt,_=>{a(f)?_(ht):a(d).length===0?_(Ct,1):_(Ht,-1)})}Rs(je,_=>c(N,_),()=>a(N));var Pt=h(je,2),Rt=o(Pt);Rs(Rt,_=>c(S,_),()=>a(S));var Ut=h(Rt,2);{var Kt=_=>{var H=Tf(),q=o(H),Q=o(q),Ye=h(q,2);at(Ye,21,()=>a($),Se=>Se.id,(Se,Ae)=>{var Tt=Pf(),gt=o(Tt);{var pt=tr=>{var Bt=Cf();C(()=>{ve(Bt,"src",a(Ae).previewUrl),ve(Bt,"alt",a(Ae).name)}),m(tr,Bt)},ie=tr=>{var Bt=Af();Bt.muted=!0,C(()=>ve(Bt,"src",a(Ae).previewUrl)),m(tr,Bt)},Ee=tr=>{var Bt=Ef(),da=o(Bt);C(Ca=>v(da,Ca),[()=>u("chat.documentFallback")]),m(tr,Bt)};W(gt,tr=>{a(Ae).isImage?tr(pt):a(Ae).isVideo?tr(ie,1):tr(Ee,-1)})}var rt=h(gt,2),er=o(rt),$r=o(er),qt=h(er,2),Xt=o(qt),Mr=h(rt,2),zr=o(Mr);C((tr,Bt)=>{v($r,a(Ae).name),v(Xt,`${a(Ae).type??""} · ${tr??""}`),v(zr,Bt)},[()=>oe(a(Ae).size),()=>u("chat.removeAttachment")]),re("click",Mr,()=>de(a(Ae).id)),m(Se,Tt)}),C(Se=>v(Q,Se),[()=>u("chat.attachments",{count:a($).length,max:r})]),m(_,H)};W(Ut,_=>{a($).length>0&&_(Kt)})}var Dt=h(Ut,2),Le=o(Dt),z=h(Le,2),me=o(z);ju(me,{size:16});var We=h(z,2),qe=o(We);C((_,H,q,Q,Ye,Se,Ae,Tt)=>{v(Ke,_),v(Ve,`${H??""}: ${l()??""}`),v(le,q),ve(ke,"aria-label",Q),_t(je,1,`flex-1 overflow-y-auto p-4 ${a(J)?"bg-blue-500/10 ring-1 ring-inset ring-blue-500/40":""}`),ve(Le,"placeholder",Ye),ve(z,"title",Se),z.disabled=a(y)||a($).length>=r,We.disabled=Ae,v(qe,Tt)},[()=>u("chat.title"),()=>u("chat.session"),()=>u("chat.back"),()=>u("chat.messagesRegion"),()=>u("chat.inputPlaceholder"),()=>u("chat.attachFiles"),()=>a(y)||!a(p).trim()&&a($).length===0,()=>a(y)?u("chat.sending"):u("chat.send")]),re("click",Ot,R),va("dragenter",ke,Y),va("dragover",ke,Qe),va("dragleave",ke,Mt),va("drop",ke,ae),va("scroll",je,xe),va("submit",Pt,j),re("change",Rt,Ce),Zr(Le,()=>a(p),_=>c(p,_)),re("click",z,ze),m(e,Z),he()}Ur(["click","change"]);var Of=k('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),Lf=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),If=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Rf=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Df=k('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-3 text-sm text-gray-500 dark:text-gray-400"> </p> <p class="mt-1 text-sm text-gray-500 dark:text-gray-400"> </p></article>'),jf=k('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),Hf=k('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function Uf(e,t){pe(t,!0);let r=L(ut([])),n=L(!0),s=L(""),i=L("");function l(E){return typeof E!="string"||E.length===0?u("common.unknown"):E.replaceAll("_"," ").split(" ").map(M=>M.charAt(0).toUpperCase()+M.slice(1)).join(" ")}function d(E){const M=`channels.names.${E}`,R=u(M);return R===M?l(E):R}async function p(){try{const E=await St.getChannelsStatus();c(r,Array.isArray(E==null?void 0:E.channels)?E.channels:[],!0),c(s,""),c(i,new Date().toLocaleTimeString(),!0)}catch(E){c(s,E instanceof Error?E.message:u("channels.loadFailed"),!0)}finally{c(n,!1)}}lr(()=>{let E=!1;const M=async()=>{E||await p()};M();const R=setInterval(M,3e4);return()=>{E=!0,clearInterval(R)}});var f=Hf(),y=o(f),w=o(y),x=o(w),O=h(w,2);{var A=E=>{var M=Of(),R=o(M);C(D=>v(R,D),[()=>u("common.updatedAt",{time:a(i)})]),m(E,M)};W(O,E=>{a(i)&&E(A)})}var N=h(y,2);{var S=E=>{var M=Lf(),R=o(M);C(D=>v(R,D),[()=>u("channels.loading")]),m(E,M)},$=E=>{var M=If(),R=o(M);C(()=>v(R,a(s))),m(E,M)},J=E=>{var M=Rf(),R=o(M);C(D=>v(R,D),[()=>u("channels.noChannels")]),m(E,M)},V=E=>{var M=jf();at(M,21,()=>a(r),It,(R,D)=>{var B=Df(),X=o(B),oe=o(X),ye=o(oe),_e=h(oe,2),G=o(_e),ne=h(X,2),ce=o(ne),de=h(ne,2),ze=o(de);C((Ce,Y,Qe,Mt,ae,xe)=>{v(ye,Ce),_t(_e,1,`rounded-full px-2 py-1 text-xs font-medium ${a(D).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),v(G,Y),v(ce,`${Qe??""}: ${Mt??""}`),v(ze,`${ae??""}: ${xe??""}`)},[()=>d(a(D).name),()=>a(D).enabled?u("common.enabled"):u("common.disabled"),()=>u("channels.type"),()=>d(a(D).type),()=>u("channels.status"),()=>d(a(D).status)]),m(R,B)}),m(E,M)};W(N,E=>{a(n)?E(S):a(s)?E($,1):a(r).length===0?E(J,2):E(V,-1)})}C(E=>v(x,E),[()=>u("channels.title")]),m(e,f),he()}const zf=e=>e;function Vf(e){const t=e-1;return t*t*t+1}function Oo(e,{delay:t=0,duration:r=400,easing:n=zf}={}){const s=+getComputedStyle(e).opacity;return{delay:t,duration:r,easing:n,css:i=>`opacity: ${i*s}`}}function Lo(e,{delay:t=0,duration:r=400,easing:n=Vf,axis:s="y"}={}){const i=getComputedStyle(e),l=+i.opacity,d=s==="y"?"height":"width",p=parseFloat(i[d]),f=s==="y"?["top","bottom"]:["left","right"],y=f.map($=>`${$[0].toUpperCase()}${$.slice(1)}`),w=parseFloat(i[`padding${y[0]}`]),x=parseFloat(i[`padding${y[1]}`]),O=parseFloat(i[`margin${y[0]}`]),A=parseFloat(i[`margin${y[1]}`]),N=parseFloat(i[`border${y[0]}Width`]),S=parseFloat(i[`border${y[1]}Width`]);return{delay:t,duration:r,easing:n,css:$=>`overflow: hidden;opacity: ${Math.min($*20,1)*l};${d}: ${$*p}px;padding-${f[0]}: ${$*w}px;padding-${f[1]}: ${$*x}px;margin-${f[0]}: ${$*O}px;margin-${f[1]}: ${$*A}px;border-${f[0]}-width: ${$*N}px;border-${f[1]}-width: ${$*S}px;min-${d}: 0`}}var qf=k('<button type="button"><span class="config-toggle__thumb svelte-18svoa7"></span></button>'),Bf=k("<option> </option>"),Kf=k('<select class="config-input svelte-18svoa7"></select>'),Wf=k('<input class="config-input svelte-18svoa7" type="number"/>'),Jf=k('<label class="tag-chip svelte-18svoa7"><input type="text" class="svelte-18svoa7"/> <button type="button" class="svelte-18svoa7">×</button></label>'),Gf=k('<div class="tag-editor svelte-18svoa7"><div class="tag-list svelte-18svoa7"></div> <button type="button" class="secondary-action svelte-18svoa7"> </button></div>'),Qf=k('<textarea class="config-editor svelte-18svoa7" rows="6"></textarea>'),Yf=k('<button type="button" class="icon-action svelte-18svoa7"><!></button>'),Xf=k('<div class="field-input-row svelte-18svoa7"><input class="config-input svelte-18svoa7"/> <!></div>'),Zf=k('<span class="config-badge svelte-18svoa7"> </span>'),ev=k('<span class="config-badge config-badge--muted svelte-18svoa7"> </span>'),tv=k('<button type="button" class="ghost-action svelte-18svoa7"><!> </button>'),rv=k('<p class="config-field__description svelte-18svoa7"> </p>'),av=k("<span> </span>"),nv=k('<article><div class="config-field__meta svelte-18svoa7"><div class="config-field__heading svelte-18svoa7"><div><div class="config-field__title-row svelte-18svoa7"><h4> </h4> <!> <!></div> <p class="config-field__path svelte-18svoa7"> </p></div> <!></div> <!> <div class="config-field__hint-row svelte-18svoa7"><span> </span> <!></div></div> <div class="config-field__control"><!></div></article>'),sv=k('<span class="config-badge svelte-18svoa7"> </span>'),ov=k('<span class="config-badge config-badge--muted svelte-18svoa7"> </span>'),iv=k('<p class="object-card__description svelte-18svoa7"> </p>'),lv=k('<p class="empty-state svelte-18svoa7"> </p>'),dv=k('<div class="object-card__grid svelte-18svoa7"></div>'),cv=k('<div class="object-card__body svelte-18svoa7"><!></div>'),uv=k('<section><button type="button" class="object-card__header svelte-18svoa7"><div><div class="object-card__title-row svelte-18svoa7"><h4> </h4> <!> <!></div> <p class="object-card__path svelte-18svoa7"> </p> <!></div> <!></button> <!></section>'),fv=k('<p class="loading-state svelte-18svoa7"> </p>'),vv=k('<p class="error-banner svelte-18svoa7"> </p>'),gv=k('<p class="inline-error svelte-18svoa7"> </p>'),pv=k('<p class="inline-error svelte-18svoa7"> </p>'),hv=k('<article class="file-card svelte-18svoa7"><div class="file-card__header svelte-18svoa7"><div><div class="file-card__title-row svelte-18svoa7"><h4> </h4> <span class="config-badge config-badge--muted svelte-18svoa7"> </span></div> <p class="svelte-18svoa7"> </p></div> <button type="button" class="primary-action svelte-18svoa7"><!> </button></div> <textarea class="config-editor svelte-18svoa7" rows="12"></textarea> <!></article>'),mv=k('<div class="advanced-grid svelte-18svoa7"><section class="advanced-card svelte-18svoa7"><div class="advanced-card__header svelte-18svoa7"><div><div class="advanced-card__title svelte-18svoa7"><!> <h3 class="svelte-18svoa7"> </h3></div> <p class="svelte-18svoa7"> </p></div> <div class="advanced-card__actions svelte-18svoa7"><button type="button" class="secondary-action svelte-18svoa7"><!> </button> <button type="button" class="primary-action svelte-18svoa7"><!> </button></div></div> <textarea class="config-editor config-editor--full svelte-18svoa7" rows="24"></textarea> <!></section> <section class="advanced-card svelte-18svoa7"><div class="advanced-card__header svelte-18svoa7"><div><div class="advanced-card__title svelte-18svoa7"><!> <h3 class="svelte-18svoa7"> </h3></div> <p class="svelte-18svoa7"> </p></div></div> <div class="file-list svelte-18svoa7"></div></section></div>'),bv=k('<span class="pill__dot svelte-18svoa7"></span>'),yv=k('<button type="button"><span> </span> <!></button>'),_v=k('<p class="empty-state svelte-18svoa7"> </p>'),xv=k('<span class="config-badge svelte-18svoa7"> </span>'),kv=k('<span class="config-badge config-badge--muted svelte-18svoa7"> </span>'),wv=k('<div class="group-card__grid svelte-18svoa7"></div>'),Sv=k('<div class="group-card__body svelte-18svoa7"><!></div>'),$v=k('<section class="group-card svelte-18svoa7"><button type="button" class="group-card__header svelte-18svoa7"><div class="group-card__title-row svelte-18svoa7"><!> <div><h3 class="svelte-18svoa7"> </h3> <p class="svelte-18svoa7"> </p></div></div> <div class="group-card__summary svelte-18svoa7"><!> <!> <!></div></button> <!></section>'),Mv=k('<div class="group-list svelte-18svoa7"></div>'),Cv=k('<div class="config-shell svelte-18svoa7"><div class="config-toolbar svelte-18svoa7"><label class="search-box svelte-18svoa7"><!> <input type="search" class="svelte-18svoa7"/></label> <div class="config-pills svelte-18svoa7"></div></div> <!></div>'),Av=k('<div class="change-row svelte-18svoa7"><span> </span> <code class="svelte-18svoa7"> </code> <span>→</span> <code class="svelte-18svoa7"> </code></div>'),Ev=k('<div class="save-bar svelte-18svoa7"><div class="save-bar__content svelte-18svoa7"><div><p class="svelte-18svoa7"> </p> <span class="svelte-18svoa7"> </span></div> <div class="save-bar__actions svelte-18svoa7"><button type="button" class="secondary-action svelte-18svoa7"> </button> <button type="button" class="primary-action svelte-18svoa7"><!> </button></div></div> <div class="save-bar__changes svelte-18svoa7"></div></div>'),Pv=k("<div> </div>"),Tv=k('<section class="config-page svelte-18svoa7"><div class="config-header svelte-18svoa7"><div><h2 class="svelte-18svoa7"> </h2> <p class="svelte-18svoa7"> </p></div> <div class="config-header__actions svelte-18svoa7"><label class="mode-switch svelte-18svoa7"><input type="checkbox" class="svelte-18svoa7"/> <span> </span></label> <button type="button" class="secondary-action svelte-18svoa7"><!> </button> <button type="button" class="secondary-action svelte-18svoa7"><!> </button></div></div> <!> <!> <!></section>');function Fv(e,t){pe(t,!0);const r=(b,g=Me)=>{const P=et(()=>ce(a(l),g().path)),I=et(()=>a(E).has(g().path));var ee=Ie(),$e=ge(ee);{var be=fe=>{var te=qf();C(()=>{_t(te,1,`config-toggle ${a(P)?"is-on":""}`,"svelte-18svoa7"),ve(te,"aria-label",g().label)}),re("click",te,()=>Ce(g().path,!a(P))),m(fe,te)},Lt=fe=>{var te=Kf();at(te,21,()=>g().enumOptions,It,(Be,it)=>{var Pe=Bf(),tt=o(Pe),Wt={};C(Je=>{v(tt,a(it).label),Wt!==(Wt=Je)&&(Pe.value=(Pe.__value=Je)??"")},[()=>String(a(it).value)]),m(Be,Pe)});var Xe;eo(te),C(()=>{Xe!==(Xe=a(P)??g().defaultValue??"")&&(te.value=(te.__value=a(P)??g().defaultValue??"")??"",Pn(te,a(P)??g().defaultValue??""))}),re("change",te,Be=>{var Pe;const it=(Pe=g().enumOptions.find(tt=>String(tt.value)===Be.currentTarget.value))==null?void 0:Pe.value;Ce(g().path,it??Be.currentTarget.value)}),m(fe,te)},$t=fe=>{var te=Wf();C(()=>{xn(te,a(P)??g().defaultValue??""),ve(te,"min",g().schema.minimum),ve(te,"max",g().schema.maximum),ve(te,"step",g().schema.multipleOf??(g().schema.type==="integer"?1:"any"))}),re("input",te,Xe=>{const Be=Xe.currentTarget.value;if(Be===""){const Pe=ye(a(l))??{};ze(Pe,g().path),c(l,Pe,!0);return}const it=g().schema.type==="integer"?parseInt(Be,10):parseFloat(Be);Number.isNaN(it)||Ce(g().path,it)}),m(fe,te)},mt=fe=>{var te=Gf(),Xe=o(te);at(Xe,21,()=>Array.isArray(a(P))?a(P):[],It,(Pe,tt,Wt)=>{var Je=Jf(),bt=o(Je),kt=h(bt,2);C(()=>xn(bt,a(tt))),re("input",bt,fr=>me(g().path,Wt,fr.currentTarget.value)),re("click",kt,()=>We(g().path,Wt)),m(Pe,Je)});var Be=h(Xe,2),it=o(Be);C(Pe=>v(it,Pe),[()=>u("config.addListItem")]),re("click",Be,()=>z(g().path)),m(fe,te)},He=fe=>{var te=Qf();C(Xe=>xn(te,Xe),[()=>JSON.stringify(a(P)??g().defaultValue??null,null,2)]),va("blur",te,Xe=>{try{Ce(g().path,JSON.parse(Xe.currentTarget.value))}catch{Xe.currentTarget.value=JSON.stringify(ce(a(l),g().path)??g().defaultValue??null,null,2)}}),m(fe,te)},xt=fe=>{var te=Xf(),Xe=o(te),Be=h(Xe,2);{var it=Pe=>{var tt=Yf(),Wt=o(tt);{var Je=kt=>{Eu(kt,{size:16})},bt=kt=>{Pu(kt,{size:16})};W(Wt,kt=>{a(I)?kt(Je):kt(bt,-1)})}C(kt=>ve(tt,"aria-label",kt),[()=>u("config.toggleVisibility")]),re("click",tt,()=>ae(g().path)),m(Pe,tt)};W(Be,Pe=>{g().sensitive&&Pe(it)})}C(Pe=>{ve(Xe,"type",g().sensitive&&!a(I)?"password":"text"),xn(Xe,a(P)??""),ve(Xe,"placeholder",Pe)},[()=>g().defaultValue!==void 0?String(g().defaultValue):""]),re("input",Xe,Pe=>Ce(g().path,Pe.currentTarget.value)),m(fe,te)};W($e,fe=>{g().inputKind==="boolean"?fe(be):g().inputKind==="enum"?fe(Lt,1):g().inputKind==="number"?fe($t,2):g().inputKind==="string-array"?fe(mt,3):g().inputKind==="json"?fe(He,4):fe(xt,-1)})}m(b,ee)},n=(b,g=Me)=>{const P=et(()=>ce(a(l),g().path));var I=nv(),ee=o(I),$e=o(ee),be=o($e),Lt=o(be),$t=o(Lt),mt=o($t),He=h($t,2);{var xt=ct=>{var yt=Zf(),rr=o(yt);C(xr=>v(rr,xr),[()=>u("config.modified")]),m(ct,yt)};W(He,ct=>{g().modifiedFromDefault&&ct(xt)})}var fe=h(He,2);{var te=ct=>{var yt=ev(),rr=o(yt);C(xr=>v(rr,xr),[()=>u("config.unsaved")]),m(ct,yt)};W(fe,ct=>{g().dirtyFromOriginal&&ct(te)})}var Xe=h(Lt,2),Be=o(Xe),it=h(be,2);{var Pe=ct=>{var yt=tv(),rr=o(yt);Fo(rr,{size:14});var xr=h(rr);C(Jt=>v(xr,` ${Jt??""}`),[()=>u("common.reset")]),re("click",yt,()=>Y(g().path,g().defaultValue)),m(ct,yt)};W(it,ct=>{g().defaultValue!==void 0&&ct(Pe)})}var tt=h($e,2);{var Wt=ct=>{var yt=rv(),rr=o(yt);C(()=>v(rr,g().description)),m(ct,yt)};W(tt,ct=>{g().description&&ct(Wt)})}var Je=h(tt,2),bt=o(Je),kt=o(bt),fr=h(bt,2);{var Vr=ct=>{var yt=av(),rr=o(yt);C((xr,Jt)=>v(rr,`${xr??""}: ${Jt??""}`),[()=>u("config.defaultValue"),()=>Nt(g().defaultValue)]),m(ct,yt)};W(fr,ct=>{g().defaultValue!==void 0&&ct(Vr)})}var vr=h(ee,2),Cr=o(vr);r(Cr,g),C((ct,yt)=>{_t(I,1,`config-field ${g().modifiedFromDefault?"is-modified":""} ${g().dirtyFromOriginal?"is-dirty":""}`,"svelte-18svoa7"),v(mt,g().label),v(Be,g().path),v(kt,`${ct??""}: ${yt??""}`)},[()=>u("config.currentValue"),()=>At(a(P))]),m(b,I)},s=(b,g=Me)=>{var P=uv(),I=o(P),ee=o(I),$e=o(ee),be=o($e),Lt=o(be),$t=h(be,2);{var mt=Je=>{var bt=sv(),kt=o(bt);C(fr=>v(kt,fr),[()=>u("config.modified")]),m(Je,bt)};W($t,Je=>{g().modifiedFromDefault&&Je(mt)})}var He=h($t,2);{var xt=Je=>{var bt=ov(),kt=o(bt);C(fr=>v(kt,fr),[()=>u("config.unsaved")]),m(Je,bt)};W(He,Je=>{g().dirtyFromOriginal&&Je(xt)})}var fe=h($e,2),te=o(fe),Xe=h(fe,2);{var Be=Je=>{var bt=iv(),kt=o(bt);C(()=>v(kt,g().description)),m(Je,bt)};W(Xe,Je=>{g().description&&Je(Be)})}var it=h(ee,2);{let Je=et(()=>`transform: rotate(${se(g())?180:0}deg); transition: transform 0.18s ease;`);To(it,{size:18,get style(){return a(Je)}})}var Pe=h(I,2);{var tt=Je=>{var bt=cv(),kt=o(bt);{var fr=vr=>{var Cr=lv(),ct=o(Cr);C(yt=>v(ct,yt),[()=>u("config.noMatchingFields")]),m(vr,Cr)},Vr=vr=>{var Cr=dv();at(Cr,21,()=>g().visibleChildren,ct=>ct.id,(ct,yt)=>{var rr=Ie(),xr=ge(rr);{var Jt=ar=>{s(ar,()=>a(yt))},ur=ar=>{n(ar,()=>a(yt))};W(xr,ar=>{a(yt).inputKind==="object"?ar(Jt):ar(ur,-1)})}m(ct,rr)}),m(vr,Cr)};W(kt,vr=>{g().visibleChildren.length===0?vr(fr):vr(Vr,-1)})}qn(3,bt,()=>Lo,()=>({duration:180})),m(Je,bt)},Wt=et(()=>se(g()));W(Pe,Je=>{a(Wt)&&Je(tt)})}C(Je=>{_t(P,1,`object-card ${g().modifiedFromDefault||g().dirtyFromOriginal?"is-emphasized":""}`,"svelte-18svoa7"),ve(I,"aria-expanded",Je),v(Lt,g().label),v(te,g().path)},[()=>se(g())]),re("click",I,()=>Fe(g().path)),m(b,P)},i=Object.freeze({});let l=L(ut({})),d=L(ut({})),p=L(null),f=L(ut([])),y=L(!0),w=L(""),x=L(""),O=L("success"),A=L(!1),N=L(!1),S=L(""),$=L("provider"),J=L(ut(new Set)),V=L(ut(new Set)),E=L(ut(new Set)),M=L(""),R=L(""),D=L(!1),B=L(ut({})),X=L(ut(i));const oe={provider:Vu,gateway:Ou,channels:Ru,agent:yu,memory:_u,security:Uu,heartbeat:Lu,reliability:Us,scheduler:$u,sessions_spawn:Nu,observability:ku,web_search:No,cost:Au,runtime:Hu,tunnel:xu,identity:bu};function ye(b){if(b!==void 0)return JSON.parse(JSON.stringify(b))}function _e(b){return b!==null&&typeof b=="object"&&!Array.isArray(b)}function G(b,g){return JSON.stringify(b)===JSON.stringify(g)}function ne(b,g){return Object.prototype.hasOwnProperty.call(b??{},g)}function ce(b,g){if(!g)return b;const P=g.split(".");let I=b;for(const ee of P)if(!_e(I)&&!Array.isArray(I)||(I=I==null?void 0:I[ee],I===void 0))return;return I}function de(b,g,P){const I=g.split(".");let ee=b;for(let $e=0;$e<I.length-1;$e+=1){const be=I[$e];_e(ee[be])||(ee[be]={}),ee=ee[be]}ee[I[I.length-1]]=P}function ze(b,g){const P=g.split(".");let I=b;for(let ee=0;ee<P.length-1;ee+=1)if(I=I==null?void 0:I[P[ee]],!_e(I))return;I&&delete I[P[P.length-1]]}function Ce(b,g){const P=ye(a(l))??{};de(P,b,g),c(l,P,!0)}function Y(b,g){g!==void 0&&Ce(b,ye(g))}function Qe(){c(l,ye(a(d))??{},!0),c(D,!1),c(R,"")}function Mt(b,g){const P=new Set(b);return P.has(g)?P.delete(g):P.add(g),P}function ae(b){c(E,Mt(a(E),b),!0)}function xe(b){c(J,Mt(a(J),b),!0)}function Fe(b){c(V,Mt(a(V),b),!0)}function ft(b){if(c($,b,!0),!a(J).has(b)){const g=new Set(a(J));g.add(b),c(J,g,!0)}Ri(b)}function jt(b){const g=String(b).toLowerCase();return["key","token","secret","password","auth","credential","private"].some(P=>g.includes(P))}function Nt(b){return b===void 0?u("config.noDefault"):typeof b=="string"?b.length>0?b:`(${u("common.empty").toLowerCase()})`:JSON.stringify(b)}function At(b){return b===void 0?`(${u("config.field.notSet").toLowerCase()})`:b===null?"null":typeof b=="string"?b.length>0?b:`(${u("common.empty").toLowerCase()})`:JSON.stringify(b)}function Vt(b,...g){return b?g.filter(I=>typeof I=="string"&&I.trim().length>0).join(" ").toLowerCase().includes(b):!0}function T(b,g){if(!g.startsWith("#/"))return null;const P=g.slice(2).split("/").map(ee=>ee.replaceAll("~1","/").replaceAll("~0","~"));let I=b;for(const ee of P)if(I=I==null?void 0:I[ee],I===void 0)return null;return I}function U(b,g){const P={...b,...g};return(b!=null&&b.properties||g!=null&&g.properties)&&(P.properties={...(b==null?void 0:b.properties)??{},...(g==null?void 0:g.properties)??{}}),(b!=null&&b.required||g!=null&&g.required)&&(P.required=Array.from(new Set([...(b==null?void 0:b.required)??[],...(g==null?void 0:g.required)??[]]))),(g==null?void 0:g.items)!==void 0?P.items=g.items:(b==null?void 0:b.items)!==void 0&&(P.items=b.items),P}function K(b){if(!b||typeof b!="object")return{};let g=b;if(g.$ref){const P=T(a(p),g.$ref);P&&(g=U(P,{...g,$ref:void 0}))}if(Array.isArray(g.allOf)&&g.allOf.length>0){let P={...g,allOf:void 0};for(const I of g.allOf)P=U(P,K(I));g=P}return g}function j(b){const g=K(b);return[...g.oneOf??[],...g.anyOf??[]].map(I=>K(I)).filter(I=>!(I.const===null||I.type==="null"||Array.isArray(I.type)&&I.type.length===1&&I.type[0]==="null"))}function Z(b,g){const P=K(b);if(Array.isArray(P.type)){const ee=P.type.filter($e=>$e!=="null");if(ee.length===1)return ee[0]}if(P.type)return P.type;if(P.properties||g&&_e(g))return"object";if(P.items||Array.isArray(g))return"array";const I=j(P);return I.length===1?Z(I[0],g):typeof g=="boolean"?"boolean":typeof g=="number"?Number.isInteger(g)?"integer":"number":typeof g=="string"?"string":null}function Ne(b){const g=K(b);if(Array.isArray(g.enum)&&g.enum.length>0)return g.enum.map(I=>({label:I===null?"(null)":String(I),value:I}));const P=[...g.oneOf??[],...g.anyOf??[]].map(I=>K(I)).filter(I=>I.const!==void 0);return P.length>0?P.map(I=>({label:I.title??(I.const===null?"(null)":String(I.const)),value:I.const})):[]}function Oe(b){return typeof b=="boolean"?{type:"boolean"}:typeof b=="number"?{type:Number.isInteger(b)?"integer":"number"}:typeof b=="string"?{type:"string"}:Array.isArray(b)?b.every(g=>typeof g=="string")?{type:"array",items:{type:"string"},default:[]}:{type:"array"}:_e(b)?{type:"object",properties:Object.fromEntries(Object.entries(b).map(([g,P])=>[g,Oe(P)]))}:{}}function Re(b,g){const P=K(b);if(Ne(P).length>0)return"enum";const ee=Z(P,g);if(ee==="boolean")return"boolean";if(ee==="number"||ee==="integer")return"number";if(ee==="string")return"string";if(ee==="object")return"object";if(ee==="array")return Z(P.items,Array.isArray(g)?g[0]:void 0)==="string"?"string-array":"json";const $e=j(P);return $e.length===2&&$e.some(be=>Z(be)==="boolean")&&$e.some(be=>Z(be)==="string")?"enum":"json"}function Ke(b,g,P,I,ee){const $e=K(g),be=$e.properties??{},Lt=Object.keys(be),$t=_e(P)?Object.keys(P):[];return[...Lt,...$t.filter(He=>!Lt.includes(He))].map(He=>{const xt=b?`${b}.${He}`:He,fe=be[He]??($e.additionalProperties&&$e.additionalProperties!==!0?$e.additionalProperties:Oe(P==null?void 0:P[He]));return we(xt,He,fe,P==null?void 0:P[He],I+1,ee)})}function we(b,g,P,I,ee,$e){const be=K(P&&Object.keys(P).length>0?P:Oe(I)),Lt=be.title??js(g),$t=be.description??"",mt=ne(be,"default")?ye(be.default):void 0,He=Re(be,I),xt=!G(I,ce(a(d),b)),fe=mt!==void 0&&!G(I,mt),te=Vt($e,g,Lt,$t,b);if(He==="object"){const Be=Ke(b,be,I,ee,$e),it=Be.filter(tt=>tt.visible),Pe=te||it.some(tt=>tt.subtreeMatches);return{id:b,path:b,key:g,label:Lt,description:$t,defaultValue:mt,dirtyFromOriginal:xt,modifiedFromDefault:fe,inputKind:He,depth:ee,children:Be,visibleChildren:it,visible:$e?Pe:!0,matchesSelf:te,subtreeMatches:Pe,sensitive:!1}}const Xe=$e?te:!0;return{id:b,path:b,key:g,label:Lt,description:$t,defaultValue:mt,currentValue:I,dirtyFromOriginal:xt,modifiedFromDefault:fe,inputKind:He,depth:ee,visible:Xe,matchesSelf:te,subtreeMatches:Xe,enumOptions:Ne(be),schema:be,sensitive:jt(g)}}function Ve(b){var ee;const g=b.trim().toLowerCase(),P=Cn(a(l)),I=((ee=a(p))==null?void 0:ee.properties)??{};return P.map($e=>{const be=$e.groupKey,Lt=I[be]??Oe(a(l)[be]),$t=we(be,be,Lt,a(l)[be],0,g),mt=es[be];return{...$e,label:(mt==null?void 0:mt.label)??$e.label,defaultOpen:(mt==null?void 0:mt.defaultOpen)??!1,icon:oe[be],node:$t}}).filter($e=>$e.node.visible)}const Ot=et(()=>Ve(a(S))),le=et(()=>a(S).trim().length>0),De=et(()=>JSON.stringify(a(l)??{},null,2)),ue=et(()=>!G(a(l),a(d))),ke=et(()=>a(Ot).filter(b=>b.node.visible));function je(b){return a(le)?b.node.subtreeMatches:a(J).has(b.groupKey)}function se(b){return a(le)?b.subtreeMatches:a(V).has(b.path)}function Et(){const b=[];function g(P,I,ee=""){const $e=_e(P),be=_e(I);if($e&&be){const Lt=Array.from(new Set([...Object.keys(P),...Object.keys(I)]));for(const $t of Lt){const mt=ee?`${ee}.${$t}`:$t;g(P[$t],I[$t],mt)}return}G(P,I)||b.push({path:ee,label:js(ee.split(".").at(-1)??ee),previous:I,current:P})}return g(a(l),a(d)),b}const vt=et(Et);function ht(b){return a(vt).some(g=>g.path===b||g.path.startsWith(`${b}.`))}function Ct(){a(D)||(c(M,a(De),!0),c(R,""))}function Ht(){var g;const b=new Set;for(const P of Cn(a(l)))(g=es[P.groupKey])!=null&&g.defaultOpen&&b.add(P.groupKey);c(J,b,!0)}function Pt(b){c(B,Object.fromEntries(b.map(g=>[g.path,g.content])),!0),c(X,i,!0)}async function Rt(){c(y,!0),c(w,"");try{const[b,g]=await Promise.all([St.getConfigSchema(),St.getConfigFiles()]);await Di({force:!0}),c(l,ye(nr.data)??{},!0),c(d,ye(nr.data)??{},!0),c(p,b??{},!0),c(f,Array.isArray(g)?g:[],!0),Pt(a(f)),Ht(),c(V,new Set,!0),c(D,!1),Ct()}catch(b){c(w,b instanceof Error?b.message:u("config.loadFailed"),!0)}finally{c(y,!1)}}async function Ut(){if(!(!a(ue)||a(A))){c(A,!0),c(x,""),c(O,"success");try{const b={};for(const P of a(vt))de(b,P.path,P.current);const g=await St.saveConfig(b);Eo(ye(a(l))??{}),c(d,ye(a(l))??{},!0),c(D,!1),Ct(),g!=null&&g.restart_required?c(x,u("config.saveRestartRequired"),!0):c(x,u("config.saveSuccess"),!0),setTimeout(()=>{c(x,"")},5e3)}catch(b){c(O,"error"),c(x,u("config.saveFailed",{message:b instanceof Error?b.message:String(b)}),!0)}finally{c(A,!1)}}}async function Kt(){if(!a(A)){c(A,!0),c(R,""),c(x,""),c(O,"success");try{const b=JSON.parse(a(M)),g=await St.saveConfig(b);c(l,ye(b)??{},!0),c(d,ye(b)??{},!0),Eo(ye(b)??{}),c(D,!1),Ct(),g!=null&&g.restart_required?c(x,u("config.saveRestartRequired"),!0):c(x,u("config.saveSuccess"),!0),setTimeout(()=>{c(x,"")},5e3)}catch(b){const g=b instanceof Error?b.message:String(b);c(R,g,!0),c(O,"error"),c(x,u("config.saveFailed",{message:g}),!0)}finally{c(A,!1)}}}async function Dt(b){const g=a(B)[b.path]??"";c(X,{...a(X),[b.path]:{saving:!0,error:""}},!0);try{const P=await St.saveConfigFile(b.filename,g);await Rt(),c(O,"success"),c(x,P!=null&&P.restart_required?u("config.saveRestartRequired"):u("config.saveSuccess"),!0),setTimeout(()=>{c(x,"")},5e3)}catch(P){c(X,{...a(X),[b.path]:{saving:!1,error:P instanceof Error?P.message:String(P)}},!0);return}c(X,{...a(X),[b.path]:{saving:!1,error:""}},!0)}async function Le(b){if(!(typeof navigator>"u"||!navigator.clipboard))try{await navigator.clipboard.writeText(b)}catch{}}function z(b){const g=ce(a(l),b),P=Array.isArray(g)?[...g,""]:[""];Ce(b,P)}function me(b,g,P){const I=ce(a(l),b),ee=Array.isArray(I)?[...I]:[];ee[g]=P,Ce(b,ee)}function We(b,g){const P=ce(a(l),b);Array.isArray(P)&&Ce(b,P.filter((I,ee)=>ee!==g))}function qe(){if(typeof window>"u")return;const b=window.location.hash.replace(/^#/,"");if(!b.startsWith("config-section-"))return;const g=b.replace(/^config-section-/,"");a(Ot).some(P=>P.groupKey===g)&&ft(g)}lr(()=>{Rt()}),lr(()=>{Ct()}),lr(()=>{a(y)||a(N)||a(Ot).length===0||queueMicrotask(()=>{qe()})});var _=Tv(),H=o(_),q=o(H),Q=o(q),Ye=o(Q),Se=h(Q,2),Ae=o(Se),Tt=h(q,2),gt=o(Tt),pt=o(gt),ie=h(pt,2),Ee=o(ie),rt=h(gt,2),er=o(rt);Mu(er,{size:14});var $r=h(er),qt=h(rt,2),Xt=o(qt);Us(Xt,{size:14});var Mr=h(Xt),zr=h(H,2);{var tr=b=>{var g=fv(),P=o(g);C(I=>v(P,I),[()=>u("config.loading")]),m(b,g)},Bt=b=>{var g=vv(),P=o(g);C(()=>v(P,a(w))),m(b,g)},da=b=>{var g=mv(),P=o(g),I=o(P),ee=o(I),$e=o(ee),be=o($e);Tu(be,{size:18});var Lt=h(be,2),$t=o(Lt),mt=h($e,2),He=o(mt),xt=h(ee,2),fe=o(xt),te=o(fe);Fo(te,{size:14});var Xe=h(te),Be=h(fe,2),it=o(Be);_s(it,{size:14});var Pe=h(it),tt=h(I,2),Wt=h(tt,2);{var Je=Jt=>{var ur=gv(),ar=o(ur);C(()=>v(ar,a(R))),m(Jt,ur)};W(Wt,Jt=>{a(R)&&Jt(Je)})}var bt=h(P,2),kt=o(bt),fr=o(kt),Vr=o(fr),vr=o(Vr);Fu(vr,{size:18});var Cr=h(vr,2),ct=o(Cr),yt=h(Vr,2),rr=o(yt),xr=h(kt,2);at(xr,21,()=>a(f),Jt=>Jt.path,(Jt,ur)=>{const ar=et(()=>a(X)[a(ur).path]);var qa=hv(),wt=o(qa),Gt=o(wt),qr=o(Gt),Br=o(qr),gn=o(Br),ca=h(Br,2),Dn=o(ca),pn=h(qr,2),hn=o(pn),Ba=h(Gt,2),jn=o(Ba);_s(jn,{size:14});var us=h(jn),mn=h(wt,2),ua=h(mn,2);{var zi=Kr=>{var Aa=pv(),Hn=o(Aa);C(()=>v(Hn,a(ar).error)),m(Kr,Aa)};W(ua,Kr=>{var Aa;(Aa=a(ar))!=null&&Aa.error&&Kr(zi)})}C((Kr,Aa)=>{var Hn;v(gn,a(ur).path),v(Dn,Kr),v(hn,a(ur).filename),Ba.disabled=(Hn=a(ar))==null?void 0:Hn.saving,v(us,` ${Aa??""}`),xn(mn,a(B)[a(ur).path]??"")},[()=>a(ur).source==="main"?u("config.sourceMain"):u("config.sourceDirectory"),()=>{var Kr;return(Kr=a(ar))!=null&&Kr.saving?u("common.saving"):u("config.saveFile")}]),re("click",Ba,()=>Dt(a(ur))),re("input",mn,Kr=>{c(B,{...a(B),[a(ur).path]:Kr.currentTarget.value},!0)}),m(Jt,qa)}),C((Jt,ur,ar,qa,wt,Gt)=>{v($t,Jt),v(He,ur),v(Xe,` ${ar??""}`),Be.disabled=a(A),v(Pe,` ${qa??""}`),v(ct,wt),v(rr,Gt)},[()=>u("config.mergedJsonTitle"),()=>u("config.mergedJsonDescription"),()=>u("common.reset"),()=>a(A)?u("common.saving"):u("config.saveJson"),()=>u("config.configFilesTitle"),()=>u("config.configFilesDescription")]),re("click",fe,()=>{c(M,a(De),!0),c(D,!1),c(R,"")}),re("click",Be,Kt),re("input",tt,()=>{c(D,!0),c(R,"")}),Zr(tt,()=>a(M),Jt=>c(M,Jt)),m(b,g)},Ca=b=>{var g=Cv(),P=o(g),I=o(P),ee=o(I);No(ee,{size:16});var $e=h(ee,2),be=h(I,2);at(be,21,()=>a(ke),He=>He.groupKey,(He,xt)=>{var fe=yv(),te=o(fe),Xe=o(te),Be=h(te,2);{var it=tt=>{var Wt=bv();m(tt,Wt)},Pe=et(()=>ht(a(xt).groupKey));W(Be,tt=>{a(Pe)&&tt(it)})}C(()=>{_t(fe,1,`pill ${a($)===a(xt).groupKey?"is-active":""}`,"svelte-18svoa7"),v(Xe,a(xt).label)}),re("click",fe,()=>ft(a(xt).groupKey)),m(He,fe)});var Lt=h(P,2);{var $t=He=>{var xt=_v(),fe=o(xt);C(te=>v(fe,te),[()=>u("config.noMatchingItems")]),m(He,xt)},mt=He=>{var xt=Mv();at(xt,21,()=>a(ke),fe=>fe.groupKey,(fe,te)=>{var Xe=$v(),Be=o(Xe),it=o(Be),Pe=o(it);{var tt=wt=>{var Gt=Ie(),qr=ge(Gt);Sd(qr,()=>a(te).icon,(Br,gn)=>{gn(Br,{size:18})}),m(wt,Gt)},Wt=wt=>{Cu(wt,{size:18})};W(Pe,wt=>{a(te).icon?wt(tt):wt(Wt,-1)})}var Je=h(Pe,2),bt=o(Je),kt=o(bt),fr=h(bt,2),Vr=o(fr),vr=h(it,2),Cr=o(vr);{var ct=wt=>{var Gt=xv(),qr=o(Gt);C(Br=>v(qr,Br),[()=>u("config.modified")]),m(wt,Gt)};W(Cr,wt=>{a(te).node.modifiedFromDefault&&wt(ct)})}var yt=h(Cr,2);{var rr=wt=>{var Gt=kv(),qr=o(Gt);C(Br=>v(qr,Br),[()=>u("config.unsaved")]),m(wt,Gt)},xr=et(()=>ht(a(te).groupKey));W(yt,wt=>{a(xr)&&wt(rr)})}var Jt=h(yt,2);{let wt=et(()=>`transform: rotate(${je(a(te))?180:0}deg); transition: transform 0.18s ease;`);To(Jt,{size:18,get style(){return a(wt)}})}var ur=h(Be,2);{var ar=wt=>{var Gt=Sv(),qr=o(Gt);{var Br=ca=>{var Dn=wv();at(Dn,21,()=>a(te).node.visibleChildren,pn=>pn.id,(pn,hn)=>{var Ba=Ie(),jn=ge(Ba);{var us=ua=>{s(ua,()=>a(hn))},mn=ua=>{n(ua,()=>a(hn))};W(jn,ua=>{a(hn).inputKind==="object"?ua(us):ua(mn,-1)})}m(pn,Ba)}),m(ca,Dn)},gn=ca=>{n(ca,()=>a(te).node)};W(qr,ca=>{a(te).node.inputKind==="object"?ca(Br):ca(gn,-1)})}qn(3,Gt,()=>Lo,()=>({duration:200})),m(wt,Gt)},qa=et(()=>je(a(te)));W(ur,wt=>{a(qa)&&wt(ar)})}C((wt,Gt)=>{ve(Xe,"id",wt),ve(Be,"aria-expanded",Gt),v(kt,a(te).label),v(Vr,a(te).groupKey)},[()=>Hs(a(te).groupKey),()=>je(a(te))]),re("click",Be,()=>{xe(a(te).groupKey),c($,a(te).groupKey,!0)}),m(fe,Xe)}),m(He,xt)};W(Lt,He=>{a(ke).length===0?He($t):He(mt,-1)})}C(He=>ve($e,"placeholder",He),[()=>u("config.searchPlaceholder")]),Zr($e,()=>a(S),He=>c(S,He)),m(b,g)};W(zr,b=>{a(y)?b(tr):a(w)?b(Bt,1):a(N)?b(da,2):b(Ca,-1)})}var Rn=h(zr,2);{var ji=b=>{var g=Ev(),P=o(g),I=o(P),ee=o(I),$e=o(ee),be=h(ee,2),Lt=o(be),$t=h(I,2),mt=o($t),He=o(mt),xt=h(mt,2),fe=o(xt);_s(fe,{size:14});var te=h(fe),Xe=h(P,2);at(Xe,21,()=>a(vt),Be=>Be.path,(Be,it)=>{var Pe=Av(),tt=o(Pe),Wt=o(tt),Je=h(tt,2),bt=o(Je),kt=h(Je,4),fr=o(kt);C((Vr,vr)=>{v(Wt,a(it).path),v(bt,Vr),v(fr,vr)},[()=>At(a(it).previous),()=>At(a(it).current)]),m(Be,Pe)}),C((Be,it,Pe,tt)=>{v($e,Be),v(Lt,it),v(He,Pe),xt.disabled=a(A),v(te,` ${tt??""}`)},[()=>u("config.unsavedChangesCount",{count:a(vt).length}),()=>u("config.saveHint"),()=>u("config.discard"),()=>a(A)?u("common.saving"):u("config.saveConfig")]),re("click",mt,Qe),re("click",xt,Ut),qn(3,g,()=>Oo),m(b,g)};W(Rn,b=>{!a(N)&&a(ue)&&!a(y)&&b(ji)})}var Hi=h(Rn,2);{var Ui=b=>{var g=Pv(),P=o(g);C(()=>{_t(g,1,`toast ${a(O)==="error"?"is-error":""}`,"svelte-18svoa7"),v(P,a(x))}),qn(3,g,()=>Oo),m(b,g)};W(Hi,b=>{a(x)&&b(Ui)})}C((b,g,P,I,ee)=>{v(Ye,b),v(Ae,g),v(Ee,P),v($r,` ${I??""}`),v(Mr,` ${ee??""}`)},[()=>u("config.title"),()=>u("config.description"),()=>u("config.advancedMode"),()=>u("config.copyJson"),()=>u("common.reload")]),Hd(pt,()=>a(N),b=>c(N,b)),re("click",rt,()=>Le(a(De))),re("click",qt,()=>Rt()),m(e,_),he()}Ur(["click","change","input"]);var Nv=k('<p class="text-gray-400 dark:text-gray-500"> </p>'),Ov=k('<li class="whitespace-pre-wrap break-words"><span class="mr-3 select-none text-gray-400 dark:text-gray-600"> </span> <span> </span></li>'),Lv=k('<ol class="space-y-1"></ol>'),Iv=k('<section class="space-y-4"><div class="flex flex-wrap items-center justify-between gap-3"><h2 class="text-2xl font-semibold"> </h2> <div class="flex items-center gap-2"><span> </span> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></div> <div class="h-[65vh] overflow-y-auto rounded-xl border border-gray-200 bg-gray-50 p-4 font-mono text-xs leading-5 text-green-800 dark:border-gray-700 dark:bg-gray-950 dark:text-green-300"><!></div></section>');function Rv(e,t){pe(t,!0);const r=1e3,n=500,s=1e4;let i=L(ut([])),l=L(!1),d=L("disconnected"),p=L(null),f=null,y=null,w=0,x=!0;const O=et(()=>a(d)==="connected"?"border-green-500/50 bg-green-500/15 text-green-700 dark:text-green-300":a(d)==="reconnecting"?"border-amber-500/50 bg-amber-500/15 text-amber-700 dark:text-amber-200":"border-red-500/50 bg-red-500/15 text-red-700 dark:text-red-300"),A=et(()=>a(d)==="connected"?u("logs.connected"):a(d)==="reconnecting"?u("logs.reconnecting"):u("logs.disconnected"));function N(){const ae=Zn?new URL(Zn,window.location.href):new URL(window.location.href);return ae.protocol=ae.protocol==="https:"?"wss:":"ws:",ae.pathname="/api/logs/stream",ae.search="",ae.hash="",ae.toString()}function S(ae){if(typeof ae!="string"||ae.length===0)return;const xe=ae.split(/\r?\n/).filter(ft=>ft.length>0);if(xe.length===0)return;const Fe=[...a(i),...xe];c(i,Fe.length>r?Fe.slice(Fe.length-r):Fe,!0)}function $(){y!==null&&(clearTimeout(y),y=null)}function J(){f&&(f.onopen=null,f.onmessage=null,f.onerror=null,f.onclose=null,f.close(),f=null)}function V(){if(!x){c(d,"disconnected");return}c(d,"reconnecting");const ae=Math.min(n*2**w,s);w+=1,$(),y=setTimeout(()=>{y=null,E()},ae)}function E(){$(),c(d,"reconnecting"),J();let ae;try{ae=new WebSocket(N())}catch{V();return}f=ae,ae.onopen=()=>{w=0,c(d,"connected")},ae.onmessage=xe=>{a(l)||S(xe.data)},ae.onerror=()=>{(ae.readyState===WebSocket.OPEN||ae.readyState===WebSocket.CONNECTING)&&ae.close()},ae.onclose=()=>{f=null,V()}}function M(){c(l,!a(l))}function R(){c(i,[],!0)}lr(()=>(x=!0,E(),()=>{x=!1,$(),J(),c(d,"disconnected")})),lr(()=>{a(i).length,a(l),!(a(l)||!a(p))&&queueMicrotask(()=>{a(p)&&(a(p).scrollTop=a(p).scrollHeight)})});var D=Iv(),B=o(D),X=o(B),oe=o(X),ye=h(X,2),_e=o(ye),G=o(_e),ne=h(_e,2),ce=o(ne),de=h(ne,2),ze=o(de),Ce=h(B,2),Y=o(Ce);{var Qe=ae=>{var xe=Nv(),Fe=o(xe);C(ft=>v(Fe,ft),[()=>u("logs.waiting")]),m(ae,xe)},Mt=ae=>{var xe=Lv();at(xe,21,()=>a(i),It,(Fe,ft,jt)=>{var Nt=Ov(),At=o(Nt),Vt=o(At),T=h(At,2),U=o(T);C(K=>{v(Vt,K),v(U,a(ft))},[()=>String(jt+1).padStart(4,"0")]),m(Fe,Nt)}),m(ae,xe)};W(Y,ae=>{a(i).length===0?ae(Qe):ae(Mt,-1)})}Rs(Ce,ae=>c(p,ae),()=>a(p)),C((ae,xe,Fe)=>{v(oe,ae),_t(_e,1,`rounded-full border px-2 py-1 text-xs font-medium uppercase tracking-wide ${a(O)}`),v(G,a(A)),v(ce,xe),v(ze,Fe)},[()=>u("logs.title"),()=>a(l)?u("logs.resume"):u("logs.pause"),()=>u("logs.clear")]),re("click",ne,M),re("click",de,R),m(e,D),he()}Ur(["click"]);var Dv=k("<option> </option>"),jv=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Hv=k('<div class="space-y-3 rounded-xl border border-sky-500/30 bg-white p-4 dark:bg-gray-800"><h3 class="text-base font-semibold text-gray-900 dark:text-gray-100"> </h3> <div class="grid gap-3 sm:grid-cols-2"><div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select></div> <div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="number" min="1000" step="1000" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="sm:col-span-2"><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="flex items-center gap-2"><span class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </span> <button type="button" disabled=""><span></span></button> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></div></div> <!> <div class="flex justify-end gap-2 pt-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"> </button></div></div>'),Uv=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),zv=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Vv=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),qv=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Bv=k("<option> </option>"),Kv=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Wv=k('<div class="space-y-3"><div class="grid gap-3 sm:grid-cols-2"><div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select></div> <div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="number" min="1000" step="1000" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="sm:col-span-2"><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="flex items-center gap-2"><span class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </span> <button type="button" disabled=""><span></span></button> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></div></div> <!> <div class="flex justify-end gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"> </button></div></div>'),Jv=k('<div class="flex items-start justify-between gap-3"><div class="min-w-0 flex-1"><div class="flex items-center gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-2 font-mono text-sm text-gray-500 dark:text-gray-400"> </p> <p class="mt-1 text-xs text-gray-400 dark:text-gray-500"> </p></div> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-2 py-1 text-xs text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-red-500/50 bg-red-500/10 px-2 py-1 text-xs text-red-600 hover:bg-red-500/20 disabled:opacity-50 dark:text-red-300"> </button></div></div>'),Gv=k('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><!></article>'),Qv=k('<!> <div class="space-y-3"></div>',1),Yv=k('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div> <div class="flex items-center gap-3 rounded-xl border border-gray-200 bg-white px-4 py-3 dark:border-gray-700 dark:bg-gray-800"><span class="text-sm font-medium text-gray-700 dark:text-gray-200"> </span> <button type="button"><span></span></button> <span class="text-xs text-gray-500 dark:text-gray-400"> </span></div> <!> <!></section>');function Xv(e,t){pe(t,!0);const r=["agent_start","agent_end","llm_request","llm_response","tool_call_start","tool_call","turn_complete","error"];let n=L(ut([])),s=L(!0),i=L(!0),l=L(""),d=L(""),p=L(null),f=L(!1),y=L(!1),w=L(""),x=L(""),O="hook-add",A=L(ut(r[0])),N=L(""),S=L(3e4),$=L(!0);function J(){c(A,r[0],!0),c(N,""),c(S,3e4),c($,!0)}function V(j,Z){return`${j}-${Z}`}function E(j){const Z=`hooks.events.${j}`,Ne=u(Z);return Ne!==Z?Ne:j.split("_").map(Oe=>Oe.charAt(0).toUpperCase()+Oe.slice(1)).join(" ")}function M(){return a(N).trim()?!Number.isFinite(Number(a(S)))||Number(a(S))<1e3?(c(d,u("hooks.timeoutInvalid"),!0),!1):!0:(c(d,u("hooks.commandRequired"),!0),!1)}async function R(){c(i,!0);try{const j=await St.getHooks();c(n,Array.isArray(j==null?void 0:j.hooks)?j.hooks:[],!0),c(s,(j==null?void 0:j.enabled)!==!1),c(l,""),c(d,"")}catch(j){c(n,[],!0),c(s,!0),c(l,j instanceof Error?j.message:u("hooks.loadFailed"),!0)}finally{c(i,!1)}}function D(j){c(p,j.id,!0),c(d,""),c(A,j.event,!0),c(N,j.command,!0),c(S,j.timeout_ms,!0),c($,j.enabled,!0)}function B(){c(p,null),c(d,""),J()}async function X(j){if(M()){c(y,!0),c(d,"");try{await St.updateHook(j,{event:a(A),command:a(N).trim(),timeout_ms:Number(a(S))}),c(p,null),J(),await R()}catch(Z){c(d,Z instanceof Error?Z.message:u("hooks.saveFailed"),!0)}finally{c(y,!1)}}}async function oe(){if(M()){c(y,!0),c(d,"");try{await St.createHook({event:a(A),command:a(N).trim(),timeout_ms:Number(a(S))}),c(f,!1),J(),await R()}catch(j){c(d,j instanceof Error?j.message:u("hooks.saveFailed"),!0)}finally{c(y,!1)}}}async function ye(j){c(w,j,!0),c(d,"");try{await St.deleteHook(j),a(p)===j&&B(),await R()}catch(Z){c(d,Z instanceof Error?Z.message:u("hooks.deleteFailed"),!0)}finally{c(w,"")}}async function _e(j){c(x,j,!0),c(d,"");try{await St.toggleHook(j),await R()}catch(Z){c(d,Z instanceof Error?Z.message:u("hooks.toggleFailed"),!0)}finally{c(x,"")}}lr(()=>{R()});var G=Yv(),ne=o(G),ce=o(ne),de=o(ce),ze=h(ce,2),Ce=o(ze),Y=h(ne,2),Qe=o(Y),Mt=o(Qe),ae=h(Qe,2),xe=o(ae),Fe=h(ae,2),ft=o(Fe),jt=h(Y,2);{var Nt=j=>{var Z=Hv(),Ne=o(Z),Oe=o(Ne),Re=h(Ne,2),Ke=o(Re),we=o(Ke),Ve=o(we),Ot=h(we,2);at(Ot,21,()=>r,It,(H,q)=>{var Q=Dv(),Ye=o(Q),Se={};C(Ae=>{v(Ye,Ae),Se!==(Se=a(q))&&(Q.value=(Q.__value=a(q))??"")},[()=>E(a(q))]),m(H,Q)});var le=h(Ke,2),De=o(le),ue=o(De),ke=h(De,2),je=h(le,2),se=o(je),Et=o(se),vt=h(se,2),ht=h(je,2),Ct=o(ht),Ht=o(Ct),Pt=h(Ct,2),Rt=o(Pt),Ut=h(Pt,2),Kt=o(Ut),Dt=h(Re,2);{var Le=H=>{var q=jv(),Q=o(q);C(()=>v(Q,a(d))),m(H,q)};W(Dt,H=>{a(d)&&H(Le)})}var z=h(Dt,2),me=o(z),We=o(me),qe=h(me,2),_=o(qe);C((H,q,Q,Ye,Se,Ae,Tt,gt,pt,ie,Ee,rt,er,$r,qt,Xt)=>{v(Oe,H),ve(we,"for",q),v(Ve,Q),ve(Ot,"id",Ye),ve(De,"for",Se),v(ue,Ae),ve(ke,"id",Tt),ve(se,"for",gt),v(Et,pt),ve(vt,"id",ie),ve(vt,"placeholder",Ee),v(Ht,rt),ve(Pt,"aria-label",er),_t(Pt,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a($)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),_t(Rt,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a($)?"translate-x-4":"translate-x-1"}`),v(Kt,$r),v(We,qt),qe.disabled=a(y),v(_,Xt)},[()=>u("hooks.newHook"),()=>V(O,"event"),()=>u("hooks.event"),()=>V(O,"event"),()=>V(O,"timeout"),()=>u("hooks.timeout"),()=>V(O,"timeout"),()=>V(O,"command"),()=>u("hooks.command"),()=>V(O,"command"),()=>u("hooks.commandPlaceholder"),()=>u("hooks.enabled"),()=>u("hooks.enabled"),()=>u("hooks.globalToggleHint"),()=>u("hooks.cancel"),()=>a(y)?u("hooks.saving"):u("hooks.save")]),Ua(Ot,()=>a(A),H=>c(A,H)),Zr(ke,()=>a(S),H=>c(S,H)),Zr(vt,()=>a(N),H=>c(N,H)),re("click",me,()=>{c(f,!1),c(d,""),J()}),re("click",qe,oe),m(j,Z)};W(jt,j=>{a(f)&&j(Nt)})}var At=h(jt,2);{var Vt=j=>{var Z=Uv(),Ne=o(Z);C(Oe=>v(Ne,Oe),[()=>u("hooks.loading")]),m(j,Z)},T=j=>{var Z=zv(),Ne=o(Z);C(()=>v(Ne,a(l))),m(j,Z)},U=j=>{var Z=Vv(),Ne=o(Z);C(Oe=>v(Ne,Oe),[()=>u("hooks.noHooks")]),m(j,Z)},K=j=>{var Z=Qv(),Ne=ge(Z);{var Oe=Ke=>{var we=qv(),Ve=o(we);C(()=>v(Ve,a(d))),m(Ke,we)};W(Ne,Ke=>{a(d)&&Ke(Oe)})}var Re=h(Ne,2);at(Re,21,()=>a(n),Ke=>Ke.id,(Ke,we)=>{var Ve=Gv(),Ot=o(Ve);{var le=ue=>{var ke=Wv(),je=o(ke),se=o(je),Et=o(se),vt=o(Et),ht=h(Et,2);at(ht,21,()=>r,It,(ie,Ee)=>{var rt=Bv(),er=o(rt),$r={};C(qt=>{v(er,qt),$r!==($r=a(Ee))&&(rt.value=(rt.__value=a(Ee))??"")},[()=>E(a(Ee))]),m(ie,rt)});var Ct=h(se,2),Ht=o(Ct),Pt=o(Ht),Rt=h(Ht,2),Ut=h(Ct,2),Kt=o(Ut),Dt=o(Kt),Le=h(Kt,2),z=h(Ut,2),me=o(z),We=o(me),qe=h(me,2),_=o(qe),H=h(qe,2),q=o(H),Q=h(je,2);{var Ye=ie=>{var Ee=Kv(),rt=o(Ee);C(()=>v(rt,a(d))),m(ie,Ee)};W(Q,ie=>{a(d)&&ie(Ye)})}var Se=h(Q,2),Ae=o(Se),Tt=o(Ae),gt=h(Ae,2),pt=o(gt);C((ie,Ee,rt,er,$r,qt,Xt,Mr,zr,tr,Bt,da,Ca,Rn)=>{ve(Et,"for",ie),v(vt,Ee),ve(ht,"id",rt),ve(Ht,"for",er),v(Pt,$r),ve(Rt,"id",qt),ve(Kt,"for",Xt),v(Dt,Mr),ve(Le,"id",zr),v(We,tr),ve(qe,"aria-label",Bt),_t(qe,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a($)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),_t(_,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a($)?"translate-x-4":"translate-x-1"}`),v(q,da),v(Tt,Ca),gt.disabled=a(y),v(pt,Rn)},[()=>V(a(we).id,"event"),()=>u("hooks.event"),()=>V(a(we).id,"event"),()=>V(a(we).id,"timeout"),()=>u("hooks.timeout"),()=>V(a(we).id,"timeout"),()=>V(a(we).id,"command"),()=>u("hooks.command"),()=>V(a(we).id,"command"),()=>u("hooks.enabled"),()=>u("hooks.enabled"),()=>u("hooks.globalToggleHint"),()=>u("hooks.cancel"),()=>a(y)?u("hooks.saving"):u("hooks.save")]),Ua(ht,()=>a(A),ie=>c(A,ie)),Zr(Rt,()=>a(S),ie=>c(S,ie)),Zr(Le,()=>a(N),ie=>c(N,ie)),re("click",Ae,B),re("click",gt,()=>X(a(we).id)),m(ue,ke)},De=ue=>{var ke=Jv(),je=o(ke),se=o(je),Et=o(se),vt=o(Et),ht=h(Et,2),Ct=o(ht),Ht=h(se,2),Pt=o(Ht),Rt=h(Ht,2),Ut=o(Rt),Kt=h(je,2),Dt=o(Kt),Le=o(Dt),z=h(Dt,2),me=o(z);C((We,qe,_,H,q)=>{v(vt,We),_t(ht,1,`rounded-full px-2 py-1 text-xs font-medium ${a(s)?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),v(Ct,qe),v(Pt,a(we).command),v(Ut,`${_??""}: ${a(we).timeout_ms??""}ms`),v(Le,H),z.disabled=a(w)===a(we).id,v(me,q)},[()=>E(a(we).event),()=>a(s)?u("common.enabled"):u("common.disabled"),()=>u("hooks.timeout"),()=>u("hooks.edit"),()=>a(w)===a(we).id?u("hooks.deleting"):u("hooks.delete")]),re("click",Dt,()=>D(a(we))),re("click",z,()=>ye(a(we).id)),m(ue,ke)};W(Ot,ue=>{a(p)===a(we).id?ue(le):ue(De,-1)})}m(Ke,Ve)}),m(j,Z)};W(At,j=>{a(i)?j(Vt):a(l)?j(T,1):a(n).length===0?j(U,2):j(K,-1)})}C((j,Z,Ne,Oe,Re)=>{v(de,j),v(Ce,Z),v(Mt,Ne),ae.disabled=a(n).length===0||a(x)!=="",ve(ae,"aria-label",Oe),_t(ae,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a(s)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),_t(xe,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(s)?"translate-x-4":"translate-x-1"}`),v(ft,Re)},[()=>u("hooks.title"),()=>a(f)?u("hooks.cancelAdd"):u("hooks.addHook"),()=>u("hooks.globalStatus"),()=>a(s)?u("common.disabled"):u("common.enabled"),()=>u("hooks.globalToggleHint")]),re("click",ze,()=>{c(f,!a(f)),c(d,""),a(f)&&J()}),re("click",ae,()=>{var j;return _e(((j=a(n)[0])==null?void 0:j.id)??"")}),m(e,G),he()}Ur(["click"]);var Zv=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),eg=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),tg=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),rg=k('<p class="mt-1 text-xs text-gray-500 dark:text-gray-400"> </p>'),ag=k('<div class="rounded-lg border border-gray-200 bg-gray-50/60 p-3 dark:border-gray-700 dark:bg-gray-900/60"><p class="font-mono text-sm font-medium text-gray-700 dark:text-gray-200"> </p> <!></div>'),ng=k('<div class="border-t border-gray-200 p-4 dark:border-gray-700"><h4 class="mb-3 text-sm font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </h4> <div class="grid gap-2"></div></div>'),sg=k('<div class="border-t border-gray-200 p-4 dark:border-gray-700"><p class="text-sm text-gray-500 dark:text-gray-400"> </p></div>'),og=k('<article class="rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><button type="button" class="flex w-full items-center justify-between gap-3 p-4 text-left"><div class="min-w-0 flex-1"><div class="flex items-center gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-1 font-mono text-sm text-gray-500 dark:text-gray-400"> </p></div> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></button> <!></article>'),ig=k('<div class="space-y-4"></div>'),lg=k('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!></section>');function dg(e,t){pe(t,!0);let r=L(ut([])),n=L(!0),s=L(""),i=L(null);async function l(){c(n,!0);try{const E=await St.getMcpServers();c(r,Array.isArray(E==null?void 0:E.servers)?E.servers:[],!0),c(s,"")}catch(E){c(r,[],!0),c(s,E instanceof Error?E.message:u("mcp.loadFailed"),!0)}finally{c(n,!1)}}function d(E){c(i,a(i)===E?null:E,!0)}async function p(){await l()}lr(()=>{l()});var f=lg(),y=o(f),w=o(y),x=o(w),O=h(w,2),A=o(O),N=h(y,2);{var S=E=>{var M=Zv(),R=o(M);C(D=>v(R,D),[()=>u("mcp.loading")]),m(E,M)},$=E=>{var M=eg(),R=o(M);C(()=>v(R,a(s))),m(E,M)},J=E=>{var M=tg(),R=o(M);C(D=>v(R,D),[()=>u("mcp.noServers")]),m(E,M)},V=E=>{var M=ig();at(M,21,()=>a(r),It,(R,D)=>{var B=og(),X=o(B),oe=o(X),ye=o(oe),_e=o(ye),G=o(_e),ne=h(_e,2),ce=o(ne),de=h(ye,2),ze=o(de),Ce=h(oe,2),Y=o(Ce),Qe=h(X,2);{var Mt=xe=>{var Fe=ng(),ft=o(Fe),jt=o(ft),Nt=h(ft,2);at(Nt,21,()=>a(D).tools,It,(At,Vt)=>{var T=ag(),U=o(T),K=o(U),j=h(U,2);{var Z=Ne=>{var Oe=rg(),Re=o(Oe);C(()=>v(Re,a(Vt).description)),m(Ne,Oe)};W(j,Ne=>{a(Vt).description&&Ne(Z)})}C(()=>v(K,a(Vt).name)),m(At,T)}),C(At=>v(jt,At),[()=>u("mcp.availableTools")]),m(xe,Fe)},ae=xe=>{var Fe=sg(),ft=o(Fe),jt=o(ft);C(Nt=>v(jt,Nt),[()=>u("mcp.noTools")]),m(xe,Fe)};W(Qe,xe=>{a(i)===a(D).name&&a(D).tools&&a(D).tools.length>0?xe(Mt):a(i)===a(D).name&&(!a(D).tools||a(D).tools.length===0)&&xe(ae,1)})}C((xe,Fe)=>{var ft;v(G,a(D).name),_t(ne,1,`rounded-full px-2 py-1 text-xs font-medium ${a(D).status==="connected"?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":a(D).status==="connecting"?"border border-yellow-500/50 bg-yellow-500/20 text-yellow-700 dark:text-yellow-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),v(ce,xe),v(ze,a(D).url),v(Y,`${((ft=a(D).tools)==null?void 0:ft.length)??0??""} ${Fe??""}`)},[()=>a(D).status==="connected"?u("mcp.connected"):a(D).status==="connecting"?u("mcp.connecting"):u("mcp.disconnected"),()=>u("mcp.tools")]),re("click",X,()=>d(a(D).name)),m(R,B)}),m(E,M)};W(N,E=>{a(n)?E(S):a(s)?E($,1):a(r).length===0?E(J,2):E(V,-1)})}C((E,M)=>{v(x,E),v(A,M)},[()=>u("mcp.title"),()=>u("common.refresh")]),re("click",O,p),m(e,f),he()}Ur(["click"]);var cg=k('<span class="text-sm text-gray-500 dark:text-gray-400"> </span>'),ug=k("<div> </div>"),fg=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),vg=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),gg=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),pg=k('<p class="mt-2 text-sm text-gray-500 dark:text-gray-400"> </p>'),hg=k('<div class="flex items-center gap-2"><span class="text-xs text-yellow-600 dark:text-yellow-400"> </span> <button type="button" class="rounded px-2 py-1 text-xs font-medium text-red-500 transition hover:bg-red-500/20 disabled:opacity-50 dark:text-red-400"> </button> <button type="button" class="rounded px-2 py-1 text-xs text-gray-500 transition hover:bg-gray-200 dark:text-gray-400 dark:hover:bg-gray-700"> </button></div>'),mg=k('<button type="button" class="rounded px-2 py-1 text-xs text-red-500 transition hover:bg-red-500/20 dark:text-red-400"> </button>'),bg=k('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3></div> <!> <p class="mt-2 font-mono text-xs text-gray-400 dark:text-gray-500"> </p> <div class="mt-3 flex items-center justify-between gap-3"><div class="flex items-center gap-2"><span> </span> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></div> <!></div></article>'),yg=k('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),_g=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),xg=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),kg=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),wg=k('<p class="mt-2 line-clamp-3 text-sm text-gray-500 dark:text-gray-400"> </p>'),Sg=k('<span class="flex items-center gap-1"><svg class="h-3.5 w-3.5" fill="currentColor" viewBox="0 0 20 20"><path d="M9.049 2.927c.3-.921 1.603-.921 1.902 0l1.07 3.292a1 1 0 00.95.69h3.462c.969 0 1.371 1.24.588 1.81l-2.8 2.034a1 1 0 00-.364 1.118l1.07 3.292c.3.921-.755 1.688-1.54 1.118l-2.8-2.034a1 1 0 00-1.175 0l-2.8 2.034c-.784.57-1.838-.197-1.539-1.118l1.07-3.292a1 1 0 00-.364-1.118L2.98 8.72c-.783-.57-.38-1.81.588-1.81h3.461a1 1 0 00.951-.69l1.07-3.292z"></path></svg> </span>'),$g=k('<span class="rounded bg-gray-100 px-1.5 py-0.5 dark:bg-gray-700"> </span>'),Mg=k('<span class="rounded-full border border-green-500/50 bg-green-500/20 px-2 py-1 text-xs font-medium text-green-700 dark:text-green-300"> </span>'),Cg=k('<button type="button" class="rounded-lg bg-sky-600 px-3 py-1 text-xs font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"> </button>'),Ag=k('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-2"><div class="min-w-0 flex-1"><h3 class="truncate text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <p class="text-xs text-gray-400 dark:text-gray-500"> </p></div> <span class="rounded-full border border-gray-300 bg-gray-100 px-2 py-0.5 text-xs text-gray-600 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-300"> </span></div> <!> <div class="mt-3 flex flex-wrap items-center gap-2 text-xs text-gray-400 dark:text-gray-500"><!> <!> <span> </span></div> <div class="mt-3 flex items-center justify-between"><a target="_blank" rel="noopener noreferrer" class="text-xs text-sky-600 hover:underline dark:text-sky-400"> </a> <!></div></article>'),Eg=k('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),Pg=k('<div class="flex flex-col gap-3 sm:flex-row"><select class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200"><option> </option></select> <input type="text" class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 placeholder-gray-400 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:placeholder-gray-500"/> <button type="button" class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"> </button></div> <!>',1),Tg=k('<section class="space-y-6"><div class="flex items-center justify-between"><div class="flex items-center gap-3"><h2 class="text-2xl font-semibold"> </h2> <!></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <div class="flex gap-1 rounded-lg border border-gray-200 bg-gray-100/50 p-1 dark:border-gray-700 dark:bg-gray-800/50"><button type="button"> </button> <button type="button"> </button></div> <!> <!> <!></section>');function Fg(e,t){pe(t,!0);let r=L("installed"),n=L(ut([])),s=L(!0),i=L(""),l=L(""),d=L("success"),p=L(ut([])),f=L(!1),y=L(""),w=L(""),x=L("github"),O=L(!1),A=L(""),N=L(""),S=L("");function $(T,U="success"){c(l,T,!0),c(d,U,!0),setTimeout(()=>{c(l,"")},3e3)}async function J(){try{const T=await St.getSkills();c(n,Array.isArray(T==null?void 0:T.skills)?T.skills:[],!0),c(i,"")}catch(T){c(n,[],!0),c(i,T instanceof Error?T.message:u("skills.loadFailed"),!0)}finally{c(s,!1)}}async function V(T){if(a(S)!==T){c(S,T,!0);return}c(S,""),c(N,T,!0);try{await St.uninstallSkill(T),c(n,a(n).filter(U=>U.name!==T),!0),$(u("skills.uninstallSuccess"))}catch(U){$(u("skills.uninstallFailed")+(U!=null&&U.message?`: ${U.message}`:""),"error")}finally{c(N,"")}}const E=et(()=>[...a(n)].sort((T,U)=>T.enabled===U.enabled?0:T.enabled?-1:1)),M=et(()=>a(n).filter(T=>T.enabled).length);async function R(){!a(w).trim()&&a(x)==="github"&&c(w,"agent skill"),c(f,!0),c(O,!0),c(y,"");try{const T=await St.discoverSkills(a(x),a(w));c(p,Array.isArray(T==null?void 0:T.results)?T.results:[],!0)}catch(T){c(p,[],!0),c(y,T instanceof Error?T.message:u("skills.searchFailed"),!0)}finally{c(f,!1)}}function D(T){return a(n).some(U=>U.name===T)}async function B(T,U){c(A,T,!0);try{const K=await St.installSkill(T,U);K!=null&&K.skill&&c(n,[...a(n),{...K.skill,enabled:!0}],!0),$(u("skills.installSuccess"))}catch(K){$(u("skills.installFailed")+(K!=null&&K.message?`: ${K.message}`:""),"error")}finally{c(A,"")}}function X(T){T.key==="Enter"&&R()}lr(()=>{J()});var oe=Tg(),ye=o(oe),_e=o(ye),G=o(_e),ne=o(G),ce=h(G,2);{var de=T=>{var U=cg(),K=o(U);C(j=>v(K,`${a(M)??""}/${a(n).length??""} ${j??""}`),[()=>u("skills.active")]),m(T,U)};W(ce,T=>{!a(s)&&a(n).length>0&&T(de)})}var ze=h(_e,2),Ce=o(ze),Y=h(ye,2),Qe=o(Y),Mt=o(Qe),ae=h(Qe,2),xe=o(ae),Fe=h(Y,2);{var ft=T=>{var U=ug(),K=o(U);C(()=>{_t(U,1,`rounded-lg px-4 py-2 text-sm ${a(d)==="error"?"border border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300":"border border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300"}`),v(K,a(l))}),m(T,U)};W(Fe,T=>{a(l)&&T(ft)})}var jt=h(Fe,2);{var Nt=T=>{var U=Ie(),K=ge(U);{var j=Re=>{var Ke=fg(),we=o(Ke);C(Ve=>v(we,Ve),[()=>u("skills.loading")]),m(Re,Ke)},Z=Re=>{var Ke=vg(),we=o(Ke);C(()=>v(we,a(i))),m(Re,Ke)},Ne=Re=>{var Ke=gg(),we=o(Ke);C(Ve=>v(we,Ve),[()=>u("skills.noSkills")]),m(Re,Ke)},Oe=Re=>{var Ke=yg();at(Ke,21,()=>a(E),It,(we,Ve)=>{var Ot=bg(),le=o(Ot),De=o(le),ue=o(De),ke=h(le,2);{var je=Le=>{var z=pg(),me=o(z);C(()=>v(me,a(Ve).description)),m(Le,z)};W(ke,Le=>{a(Ve).description&&Le(je)})}var se=h(ke,2),Et=o(se),vt=h(se,2),ht=o(vt),Ct=o(ht),Ht=o(Ct),Pt=h(Ct,2),Rt=o(Pt),Ut=h(ht,2);{var Kt=Le=>{var z=hg(),me=o(z),We=o(me),qe=h(me,2),_=o(qe),H=h(qe,2),q=o(H);C((Q,Ye,Se)=>{v(We,Q),qe.disabled=a(N)===a(Ve).name,v(_,Ye),v(q,Se)},[()=>u("skills.confirmUninstall").replace("{name}",a(Ve).name),()=>a(N)===a(Ve).name?u("skills.uninstalling"):u("common.yes"),()=>u("common.no")]),re("click",qe,()=>V(a(Ve).name)),re("click",H,()=>{c(S,"")}),m(Le,z)},Dt=Le=>{var z=mg(),me=o(z);C(We=>v(me,We),[()=>u("skills.uninstall")]),re("click",z,()=>V(a(Ve).name)),m(Le,z)};W(Ut,Le=>{a(S)===a(Ve).name?Le(Kt):Le(Dt,-1)})}C((Le,z)=>{v(ue,a(Ve).name),v(Et,a(Ve).location),_t(Ct,1,`rounded-full px-2 py-1 text-xs font-medium ${a(Ve).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),v(Ht,Le),v(Rt,z)},[()=>a(Ve).enabled?u("common.enabled"):u("common.disabled"),()=>u("skills.readOnlyState")]),m(we,Ot)}),m(Re,Ke)};W(K,Re=>{a(s)?Re(j):a(i)?Re(Z,1):a(n).length===0?Re(Ne,2):Re(Oe,-1)})}m(T,U)};W(jt,T=>{a(r)==="installed"&&T(Nt)})}var At=h(jt,2);{var Vt=T=>{var U=Pg(),K=ge(U),j=o(K),Z=o(j),Ne=o(Z);Z.value=Z.__value="github";var Oe=h(j,2),Re=h(Oe,2),Ke=o(Re),we=h(K,2);{var Ve=ue=>{var ke=_g(),je=o(ke);C(se=>v(je,se),[()=>u("skills.searching")]),m(ue,ke)},Ot=ue=>{var ke=xg(),je=o(ke);C(()=>v(je,a(y))),m(ue,ke)},le=ue=>{var ke=kg(),je=o(ke);C(se=>v(je,se),[()=>u("skills.noResults")]),m(ue,ke)},De=ue=>{var ke=Eg();at(ke,21,()=>a(p),It,(je,se)=>{const Et=et(()=>D(a(se).name));var vt=Ag(),ht=o(vt),Ct=o(ht),Ht=o(Ct),Pt=o(Ht),Rt=h(Ht,2),Ut=o(Rt),Kt=h(Ct,2),Dt=o(Kt),Le=h(ht,2);{var z=ie=>{var Ee=wg(),rt=o(Ee);C(()=>v(rt,a(se).description)),m(ie,Ee)};W(Le,ie=>{a(se).description&&ie(z)})}var me=h(Le,2),We=o(me);{var qe=ie=>{var Ee=Sg(),rt=h(o(Ee));C(()=>v(rt,` ${a(se).stars??""}`)),m(ie,Ee)};W(We,ie=>{a(se).stars>0&&ie(qe)})}var _=h(We,2);{var H=ie=>{var Ee=$g(),rt=o(Ee);C(()=>v(rt,a(se).language)),m(ie,Ee)};W(_,ie=>{a(se).language&&ie(H)})}var q=h(_,2),Q=o(q),Ye=h(me,2),Se=o(Ye),Ae=o(Se),Tt=h(Se,2);{var gt=ie=>{var Ee=Mg(),rt=o(Ee);C(er=>v(rt,er),[()=>u("skills.installed")]),m(ie,Ee)},pt=ie=>{var Ee=Cg(),rt=o(Ee);C(er=>{Ee.disabled=a(A)===a(se).url,v(rt,er)},[()=>a(A)===a(se).url?u("skills.installing"):u("skills.install")]),re("click",Ee,()=>B(a(se).url,a(se).name)),m(ie,Ee)};W(Tt,ie=>{a(Et)?ie(gt):ie(pt,-1)})}C((ie,Ee,rt)=>{v(Pt,a(se).name),v(Ut,`${ie??""} ${a(se).owner??""}`),v(Dt,a(se).source),_t(q,1,Zs(a(se).has_license?"text-green-600 dark:text-green-400":"text-yellow-600 dark:text-yellow-400")),v(Q,Ee),ve(Se,"href",a(se).url),v(Ae,rt)},[()=>u("skills.owner"),()=>a(se).has_license?u("skills.licensed"):u("skills.unlicensed"),()=>a(se).url.replace("https://github.com/","")]),m(je,vt)}),m(ue,ke)};W(we,ue=>{a(f)?ue(Ve):a(y)?ue(Ot,1):a(O)&&a(p).length===0?ue(le,2):a(p).length>0&&ue(De,3)})}C((ue,ke,je)=>{v(Ne,ue),ve(Oe,"placeholder",ke),Re.disabled=a(f),v(Ke,je)},[()=>u("skills.sources.github"),()=>u("skills.search"),()=>a(f)?u("skills.searching"):u("skills.searchBtn")]),Ua(j,()=>a(x),ue=>c(x,ue)),re("keydown",Oe,X),Zr(Oe,()=>a(w),ue=>c(w,ue)),re("click",Re,R),m(T,U)};W(At,T=>{a(r)==="discover"&&T(Vt)})}C((T,U,K,j)=>{v(ne,T),v(Ce,U),_t(Qe,1,`rounded-md px-4 py-2 text-sm font-medium transition ${a(r)==="installed"?"bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white":"text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"}`),v(Mt,K),_t(ae,1,`rounded-md px-4 py-2 text-sm font-medium transition ${a(r)==="discover"?"bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white":"text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"}`),v(xe,j)},[()=>u("skills.title"),()=>u("common.refresh"),()=>u("skills.tabInstalled"),()=>u("skills.tabDiscover")]),re("click",ze,()=>{c(s,!0),J()}),re("click",Qe,()=>{c(r,"installed")}),re("click",ae,()=>{c(r,"discover")}),m(e,oe),he()}Ur(["click","keydown"]);var Ng=k("<div> </div>"),Og=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Lg=k('<div class="rounded-lg border border-red-200 bg-red-50 p-4 text-sm text-red-600 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400"> </div>'),Ig=k('<div class="rounded-lg border border-gray-200 bg-gray-50 p-8 text-center dark:border-gray-700 dark:bg-gray-800"><!> <p class="text-sm text-gray-500 dark:text-gray-400"> </p></div>'),Rg=k('<p class="mb-3 text-sm text-gray-600 dark:text-gray-300"> </p>'),Dg=k('<span class="rounded-full bg-sky-100 px-2 py-0.5 text-xs text-sky-700 dark:bg-sky-900/30 dark:text-sky-300"> </span>'),jg=k('<div class="mb-3"><p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400"> </p> <div class="flex flex-wrap gap-1"></div></div>'),Hg=k('<span class="rounded-full bg-amber-100 px-2 py-0.5 text-xs text-amber-700 dark:bg-amber-900/30 dark:text-amber-300"> </span>'),Ug=k('<div class="mb-3"><p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400"> </p> <div class="flex flex-wrap gap-1"></div></div>'),zg=k('<div class="rounded-lg border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="mb-3 flex items-start justify-between"><div><h3 class="font-semibold text-gray-900 dark:text-gray-100"> </h3> <p class="text-xs text-gray-500 dark:text-gray-400"> </p></div> <div><!> <span class="text-xs"> </span></div></div> <!> <!> <!> <div class="flex justify-end"><button type="button" class="flex items-center gap-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-xs text-gray-700 transition hover:bg-gray-100 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200 dark:hover:bg-gray-600"><!> </button></div></div>'),Vg=k('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),qg=k('<!> <section class="space-y-6"><div class="flex items-center justify-between"><div class="flex items-center gap-2"><!> <h2 class="text-2xl font-semibold"> </h2></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!></section>',1);function Bg(e,t){pe(t,!0);let r=L(ut([])),n=L(!0),s=L(""),i=L(""),l=L(""),d=L("success");function p(G,ne="success"){c(l,G,!0),c(d,ne,!0),setTimeout(()=>{c(l,"")},3e3)}async function f(){c(n,!0);try{const G=await St.getPlugins();c(r,Array.isArray(G==null?void 0:G.plugins)?G.plugins:[],!0),c(s,"")}catch{c(r,[],!0),c(s,u("plugins.loadFailed"),!0)}finally{c(n,!1)}}async function y(G){c(i,G,!0);try{await St.reloadPlugin(G),p(u("plugins.reloadSuccess",{name:G})),await f()}catch(ne){p(u("plugins.reloadFailed")+(ne.message?`: ${ne.message}`:""),"error")}finally{c(i,"")}}function w(G){return typeof G=="string"&&G==="Active"?"text-green-500":typeof G=="object"&&(G!=null&&G.Error)?"text-red-500":"text-yellow-500"}function x(G){return typeof G=="string"&&G==="Active"?u("plugins.statusActive"):typeof G=="object"&&(G!=null&&G.Error)?G.Error:u("common.unknown")}lr(()=>{f()});var O=qg(),A=ge(O);{var N=G=>{var ne=Ng(),ce=o(ne);C(()=>{_t(ne,1,`fixed right-4 top-4 z-50 rounded-lg px-4 py-2 text-sm font-medium text-white shadow-lg transition ${a(d)==="error"?"bg-red-600":"bg-green-600"}`),v(ce,a(l))}),m(G,ne)};W(A,G=>{a(l)&&G(N)})}var S=h(A,2),$=o(S),J=o($),V=o(J);Po(V,{size:24});var E=h(V,2),M=o(E),R=h(J,2),D=o(R),B=h($,2);{var X=G=>{var ne=Og(),ce=o(ne);C(de=>v(ce,de),[()=>u("plugins.loading")]),m(G,ne)},oe=G=>{var ne=Lg(),ce=o(ne);C(()=>v(ce,a(s))),m(G,ne)},ye=G=>{var ne=Ig(),ce=o(ne);Po(ce,{size:40,class:"mx-auto mb-3 text-gray-400 dark:text-gray-500"});var de=h(ce,2),ze=o(de);C(Ce=>v(ze,Ce),[()=>u("plugins.noPlugins")]),m(G,ne)},_e=G=>{var ne=Vg();at(ne,21,()=>a(r),It,(ce,de)=>{var ze=zg(),Ce=o(ze),Y=o(Ce),Qe=o(Y),Mt=o(Qe),ae=h(Qe,2),xe=o(ae),Fe=h(Y,2),ft=o(Fe);{var jt=le=>{Su(le,{size:16})},Nt=le=>{wu(le,{size:16})};W(ft,le=>{typeof a(de).status=="string"&&a(de).status==="Active"?le(jt):le(Nt,-1)})}var At=h(ft,2),Vt=o(At),T=h(Ce,2);{var U=le=>{var De=Rg(),ue=o(De);C(()=>v(ue,a(de).description)),m(le,De)};W(T,le=>{a(de).description&&le(U)})}var K=h(T,2);{var j=le=>{var De=jg(),ue=o(De),ke=o(ue),je=h(ue,2);at(je,21,()=>a(de).capabilities,It,(se,Et)=>{var vt=Dg(),ht=o(vt);C(()=>v(ht,a(Et))),m(se,vt)}),C(se=>v(ke,se),[()=>u("plugins.capabilities")]),m(le,De)};W(K,le=>{var De;(De=a(de).capabilities)!=null&&De.length&&le(j)})}var Z=h(K,2);{var Ne=le=>{var De=Ug(),ue=o(De),ke=o(ue),je=h(ue,2);at(je,21,()=>a(de).permissions_required,It,(se,Et)=>{var vt=Hg(),ht=o(vt);C(()=>v(ht,a(Et))),m(se,vt)}),C(se=>v(ke,se),[()=>u("plugins.permissions")]),m(le,De)};W(Z,le=>{var De;(De=a(de).permissions_required)!=null&&De.length&&le(Ne)})}var Oe=h(Z,2),Re=o(Oe),Ke=o(Re);{var we=le=>{Iu(le,{size:14,class:"animate-spin"})},Ve=le=>{Us(le,{size:14})};W(Ke,le=>{a(i)===a(de).name?le(we):le(Ve,-1)})}var Ot=h(Ke);C((le,De,ue)=>{v(Mt,a(de).name),v(xe,`v${a(de).version??""}`),_t(Fe,1,`flex items-center gap-1 ${le??""}`),v(Vt,De),Re.disabled=a(i)===a(de).name,v(Ot,` ${ue??""}`)},[()=>w(a(de).status),()=>x(a(de).status),()=>u("plugins.reload")]),re("click",Re,()=>y(a(de).name)),m(ce,ze)}),m(G,ne)};W(B,G=>{a(n)?G(X):a(s)?G(oe,1):a(r).length===0?G(ye,2):G(_e,-1)})}C((G,ne)=>{v(M,G),v(D,ne)},[()=>u("plugins.title"),()=>u("common.refresh")]),re("click",R,f),m(e,O),he()}Ur(["click"]);var Kg=k('<button type="button" class="fixed inset-0 z-30 bg-black/30 dark:bg-black/60 lg:hidden"></button>'),Wg=k('<button type="button"> </button>'),Jg=k('<p class="px-2 py-1 text-xs text-gray-400 dark:text-gray-500"> </p>'),Gg=k('<div class="ml-4 mt-1 space-y-1 border-l border-gray-200 pl-3 dark:border-gray-700"><!> <!></div>'),Qg=k('<button type="button"> </button> <!>',1),Yg=k("<option> </option>"),Xg=k('<section class="space-y-4"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></section>'),Zg=k('<div class="flex min-h-screen"><!> <aside><div class="mb-4 border-b border-gray-200 pb-4 dark:border-gray-700"><p class="text-lg font-semibold"> </p></div> <nav class="space-y-1"></nav></aside> <div class="flex min-w-0 flex-1 flex-col"><header class="console-header sticky top-0 z-20 flex items-center justify-between border-b border-gray-200 bg-white/95 px-4 py-3 backdrop-blur dark:border-gray-700 dark:bg-gray-900/95"><div class="flex items-center gap-3"><button type="button" class="rounded-lg border border-gray-300 px-2 py-1 text-sm text-gray-700 dark:border-gray-700 dark:text-gray-200 lg:hidden"> </button> <h1 class="text-lg font-semibold"> </h1></div> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white p-2 text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"><!></button> <label class="sr-only" for="app-language-select"> </label> <select id="app-language-select" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"></select> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></header> <main class="flex-1 p-4 sm:p-6"><!></main></div></div>'),e0=k('<div class="console-shell min-h-screen bg-gray-50 text-gray-900 dark:bg-gray-900 dark:text-gray-100"><!></div>');function t0(e,t){pe(t,!0);let r=L(ut(Ii())),n=L(ut(Xn())),s=L(!1);const i="prx-console-theme";let l=L("system"),d=L(!0),p=L(ut([])),f=L(!1),y=L(ut(typeof window<"u"?window.location.hash:""));const w=et(()=>a(n).length>0),x=et(()=>a(w)&&a(r)==="/"?"/overview":a(r)),O=et(()=>a(x).startsWith("/chat/")?"/sessions":a(x)),A=et(()=>a(x)==="/config"),N=et(()=>no.map(Y=>({value:Y,label:u(`languages.${Y}`)}))),S=et(()=>a(y).startsWith("#config-section-")?a(y).slice(16):"");function $(Y){try{return decodeURIComponent(Y)}catch{return Y}}const J=et(()=>a(x).startsWith("/chat/")?$(a(x).slice(6)):"");function V(){return a(l)==="dark"?!0:a(l)==="light"?!1:window.matchMedia("(prefers-color-scheme: dark)").matches}function E(){const Y=localStorage.getItem(i);c(l,Y==="light"||Y==="dark"?Y:"system",!0),M()}function M(){const Y=V();c(d,Y,!0),document.documentElement.classList.toggle("dark",Y),document.documentElement.classList.toggle("light",!Y),document.documentElement.style.colorScheme=Y?"dark":"light"}function R(){c(l,a(d)?"light":"dark",!0),localStorage.setItem(i,a(l)),M()}function D(){c(n,Xn(),!0),Mo()}function B(Y){c(r,Y,!0),c(s,!1),c(y,typeof window<"u"?window.location.hash:"",!0)}function X(Y){c(n,Y,!0),ga("/overview",!0)}function oe(){Oi(),c(n,""),ga("/",!0)}function ye(Y){ga(Y)}function _e(){c(y,window.location.hash,!0)}async function G(){if(!(!a(w)||a(x)!=="/config"||a(f))){c(f,!0);try{const Y=await Di();c(p,Cn(Y),!0)}catch{c(p,Cn(null),!0)}finally{c(f,!1)}}}function ne(Y){Ri(Y),c(s,!1)}lr(()=>{E(),Mo();const Y=du(B),Qe=window.matchMedia("(prefers-color-scheme: dark)"),Mt=xe=>{if(xe.key==="prx-console-token"){D();return}if(xe.key===cs&&lu(),xe.key===i){const Fe=localStorage.getItem(i);c(l,Fe==="light"||Fe==="dark"?Fe:"system",!0),M()}},ae=()=>{a(l)==="system"&&M()};return window.addEventListener("storage",Mt),window.addEventListener("hashchange",_e),Qe.addEventListener("change",ae),()=>{Y(),window.removeEventListener("storage",Mt),window.removeEventListener("hashchange",_e),Qe.removeEventListener("change",ae)}}),lr(()=>{if(a(w)&&a(r)==="/"){ga("/overview",!0);return}!a(w)&&a(r)!=="/"&&ga("/",!0)}),lr(()=>{if(a(A)){nr.data&&c(p,Cn(nr.data),!0),G();return}c(p,[],!0)});var ce=e0(),de=o(ce);{var ze=Y=>{Wu(Y,{onLogin:X})},Ce=Y=>{var Qe=Zg(),Mt=o(Qe);{var ae=z=>{var me=Kg();C(We=>ve(me,"aria-label",We),[()=>u("app.closeSidebar")]),re("click",me,()=>c(s,!1)),m(z,me)};W(Mt,z=>{a(s)&&z(ae)})}var xe=h(Mt,2),Fe=o(xe),ft=o(Fe),jt=o(ft),Nt=h(Fe,2);at(Nt,21,()=>Wd,It,(z,me)=>{var We=Qg(),qe=ge(We),_=o(qe),H=h(qe,2);{var q=Q=>{var Ye=Gg(),Se=o(Ye);at(Se,17,()=>a(p),It,(gt,pt)=>{var ie=Wg(),Ee=o(ie);C(()=>{_t(ie,1,`w-full rounded-md px-2 py-1.5 text-left text-xs transition ${a(S)===a(pt).groupKey?"bg-sky-50 text-sky-700 dark:bg-sky-500/10 dark:text-sky-300":"text-gray-500 hover:bg-gray-100 hover:text-gray-800 dark:text-gray-400 dark:hover:bg-gray-700 dark:hover:text-gray-100"}`),v(Ee,a(pt).label)}),re("click",ie,()=>ne(a(pt).groupKey)),m(gt,ie)});var Ae=h(Se,2);{var Tt=gt=>{var pt=Jg(),ie=o(pt);C(Ee=>v(ie,Ee),[()=>u("common.loading")]),m(gt,pt)};W(Ae,gt=>{a(f)&&a(p).length===0&&gt(Tt)})}m(Q,Ye)};W(H,Q=>{a(me).path==="/config"&&a(A)&&Q(q)})}C(Q=>{_t(qe,1,`w-full rounded-lg px-3 py-2 text-left text-sm transition ${a(O)===a(me).path?"bg-sky-600 text-white":"text-gray-600 hover:bg-gray-100 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-gray-100"}`),v(_,Q)},[()=>u(a(me).labelKey)]),re("click",qe,()=>ye(a(me).path)),m(z,We)});var At=h(xe,2),Vt=o(At),T=o(Vt),U=o(T),K=o(U),j=h(U,2),Z=o(j),Ne=h(T,2),Oe=o(Ne),Re=o(Oe);{var Ke=z=>{zu(z,{size:16})},we=z=>{Du(z,{size:16})};W(Re,z=>{a(d)?z(Ke):z(we,-1)})}var Ve=h(Oe,2),Ot=o(Ve),le=h(Ve,2);at(le,21,()=>a(N),It,(z,me)=>{var We=Yg(),qe=o(We),_={};C(()=>{v(qe,a(me).label),_!==(_=a(me).value)&&(We.value=(We.__value=a(me).value)??"")}),m(z,We)});var De=h(le,2),ue=o(De),ke=h(Vt,2),je=o(ke);{var se=z=>{nf(z,{})},Et=z=>{hf(z,{})},vt=z=>{Nf(z,{get sessionId(){return a(J)}})},ht=et(()=>a(x).startsWith("/chat/")),Ct=z=>{Uf(z,{})},Ht=z=>{Xv(z,{})},Pt=z=>{dg(z,{})},Rt=z=>{Fg(z,{})},Ut=z=>{Bg(z,{})},Kt=z=>{Fv(z,{})},Dt=z=>{Rv(z,{})},Le=z=>{var me=Xg(),We=o(me),qe=o(We),_=h(We,2),H=o(_);C((q,Q)=>{v(qe,q),v(H,Q)},[()=>u("app.notFound"),()=>u("app.backToOverview")]),re("click",_,()=>ye("/overview")),m(z,me)};W(je,z=>{a(x)==="/overview"?z(se):a(x)==="/sessions"?z(Et,1):a(ht)?z(vt,2):a(x)==="/channels"?z(Ct,3):a(x)==="/hooks"?z(Ht,4):a(x)==="/mcp"?z(Pt,5):a(x)==="/skills"?z(Rt,6):a(x)==="/plugins"?z(Ut,7):a(x)==="/config"?z(Kt,8):a(x)==="/logs"?z(Dt,9):z(Le,-1)})}C((z,me,We,qe,_,H,q)=>{_t(xe,1,`console-sidebar fixed inset-y-0 left-0 z-40 w-64 border-r border-gray-200 bg-white p-4 transition-transform dark:border-gray-700 dark:bg-gray-800 lg:static lg:translate-x-0 ${a(s)?"translate-x-0":"-translate-x-full"}`),v(jt,z),v(K,me),v(Z,We),ve(Oe,"aria-label",qe),v(Ot,_),ve(le,"aria-label",H),v(ue,q)},[()=>u("app.title"),()=>u("app.menu"),()=>u("app.title"),()=>u("app.theme"),()=>u("app.language"),()=>u("app.language"),()=>u("common.logout")]),re("click",U,()=>c(s,!a(s))),re("click",Oe,R),re("change",le,z=>so(z.currentTarget.value)),Ua(le,()=>$a.lang,z=>$a.lang=z),re("click",De,oe),m(Y,Qe)};W(de,Y=>{a(w)?Y(Ce,-1):Y(ze)})}m(e,ce),he()}Ur(["click","change"]);bd(t0,{target:document.getElementById("app")});
