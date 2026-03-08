var Is=Object.defineProperty;var oo=e=>{throw TypeError(e)};var Ls=(e,t,r)=>t in e?Is(e,t,{enumerable:!0,configurable:!0,writable:!0,value:r}):e[t]=r;var er=(e,t,r)=>Ls(e,typeof t!="symbol"?t+"":t,r),gn=(e,t,r)=>t.has(e)||oo("Cannot "+r);var $=(e,t,r)=>(gn(e,t,"read from private field"),r?r.call(e):t.get(e)),We=(e,t,r)=>t.has(e)?oo("Cannot add the same private member more than once"):t instanceof WeakSet?t.add(e):t.set(e,r),Fe=(e,t,r,n)=>(gn(e,t,"write to private field"),n?n.call(e,r):t.set(e,r),r),$t=(e,t,r)=>(gn(e,t,"access private method"),r);(function(){const t=document.createElement("link").relList;if(t&&t.supports&&t.supports("modulepreload"))return;for(const o of document.querySelectorAll('link[rel="modulepreload"]'))n(o);new MutationObserver(o=>{for(const s of o)if(s.type==="childList")for(const l of s.addedNodes)l.tagName==="LINK"&&l.rel==="modulepreload"&&n(l)}).observe(document,{childList:!0,subtree:!0});function r(o){const s={};return o.integrity&&(s.integrity=o.integrity),o.referrerPolicy&&(s.referrerPolicy=o.referrerPolicy),o.crossOrigin==="use-credentials"?s.credentials="include":o.crossOrigin==="anonymous"?s.credentials="omit":s.credentials="same-origin",s}function n(o){if(o.ep)return;o.ep=!0;const s=r(o);fetch(o.href,s)}})();const wn=!1;var Bn=Array.isArray,Rs=Array.prototype.indexOf,va=Array.prototype.includes,rn=Array.from,js=Object.defineProperty,Pr=Object.getOwnPropertyDescriptor,Hs=Object.getOwnPropertyDescriptors,Ds=Object.prototype,Us=Array.prototype,Mo=Object.getPrototypeOf,so=Object.isExtensible;function Aa(e){return typeof e=="function"}const Ce=()=>{};function zs(e){for(var t=0;t<e.length;t++)e[t]()}function No(){var e,t,r=new Promise((n,o)=>{e=n,t=o});return{promise:r,resolve:e,reject:t}}function Na(e,t){if(Array.isArray(e))return e;if(!(Symbol.iterator in e))return Array.from(e);const r=[];for(const n of e)if(r.push(n),r.length===t)break;return r}const Ft=2,_a=4,ga=8,an=1<<24,jr=16,sr=32,Zr=64,Sn=128,Xt=512,Tt=1024,Pt=2048,or=4096,jt=8192,gr=16384,xa=32768,Sr=65536,io=1<<17,Bs=1<<18,ka=1<<19,Ws=1<<20,fr=1<<25,Yr=65536,En=1<<21,Wn=1<<22,Or=1<<23,Fr=Symbol("$state"),To=Symbol("legacy props"),qs=Symbol(""),Hr=new class extends Error{constructor(){super(...arguments);er(this,"name","StaleReactionError");er(this,"message","The reaction that called `getAbortSignal()` was re-run or destroyed")}};var Ao;const qn=!!((Ao=globalThis.document)!=null&&Ao.contentType)&&globalThis.document.contentType.includes("xml");function Po(e){throw new Error("https://svelte.dev/e/lifecycle_outside_component")}function Vs(){throw new Error("https://svelte.dev/e/async_derived_orphan")}function Gs(e,t,r){throw new Error("https://svelte.dev/e/each_key_duplicate")}function Ks(e){throw new Error("https://svelte.dev/e/effect_in_teardown")}function Js(){throw new Error("https://svelte.dev/e/effect_in_unowned_derived")}function Ys(e){throw new Error("https://svelte.dev/e/effect_orphan")}function Xs(){throw new Error("https://svelte.dev/e/effect_update_depth_exceeded")}function Qs(e){throw new Error("https://svelte.dev/e/props_invalid_value")}function Zs(){throw new Error("https://svelte.dev/e/state_descriptors_fixed")}function ei(){throw new Error("https://svelte.dev/e/state_prototype_fixed")}function ti(){throw new Error("https://svelte.dev/e/state_unsafe_mutation")}function ri(){throw new Error("https://svelte.dev/e/svelte_boundary_reset_onerror")}const ai=1,ni=2,Oo=4,oi=8,si=16,ii=1,li=4,di=8,ci=16,ui=1,fi=2,Mt=Symbol(),Fo="http://www.w3.org/1999/xhtml",Io="http://www.w3.org/2000/svg",vi="http://www.w3.org/1998/Math/MathML",gi="@attach";function pi(){console.warn("https://svelte.dev/e/select_multiple_invalid_value")}function yi(){console.warn("https://svelte.dev/e/svelte_boundary_reset_noop")}function Lo(e){return e===this.v}function bi(e,t){return e!=e?t==t:e!==t||e!==null&&typeof e=="object"||typeof e=="function"}function Ro(e){return!bi(e,this.v)}let hi=!1,Wt=null;function pa(e){Wt=e}function Te(e,t=!1,r){Wt={p:Wt,i:!1,c:null,e:null,s:e,x:null,l:null}}function Pe(e){var t=Wt,r=t.e;if(r!==null){t.e=null;for(var n of r)ns(n)}return t.i=!0,Wt=t.p,{}}function jo(){return!0}let Dr=[];function Ho(){var e=Dr;Dr=[],zs(e)}function pr(e){if(Dr.length===0&&!Pa){var t=Dr;queueMicrotask(()=>{t===Dr&&Ho()})}Dr.push(e)}function mi(){for(;Dr.length>0;)Ho()}function Do(e){var t=Je;if(t===null)return Ue.f|=Or,e;if(!(t.f&xa)&&!(t.f&_a))throw e;Tr(e,t)}function Tr(e,t){for(;t!==null;){if(t.f&Sn){if(!(t.f&xa))throw e;try{t.b.error(e);return}catch(r){e=r}}t=t.parent}throw e}const _i=-7169;function wt(e,t){e.f=e.f&_i|t}function Vn(e){e.f&Xt||e.deps===null?wt(e,Tt):wt(e,or)}function Uo(e){if(e!==null)for(const t of e)!(t.f&Ft)||!(t.f&Yr)||(t.f^=Yr,Uo(t.deps))}function zo(e,t,r){e.f&Pt?t.add(e):e.f&or&&r.add(e),Uo(e.deps),wt(e,Tt)}const Wa=new Set;let Me=null,Ya=null,Nt=null,Ut=[],nn=null,Pa=!1,ya=null,xi=1;var Cr,oa,Wr,sa,ia,la,Mr,lr,da,qt,An,$n,Cn,Mn;const no=class no{constructor(){We(this,qt);er(this,"id",xi++);er(this,"current",new Map);er(this,"previous",new Map);We(this,Cr,new Set);We(this,oa,new Set);We(this,Wr,0);We(this,sa,0);We(this,ia,null);We(this,la,new Set);We(this,Mr,new Set);We(this,lr,new Map);er(this,"is_fork",!1);We(this,da,!1)}skip_effect(t){$(this,lr).has(t)||$(this,lr).set(t,{d:[],m:[]})}unskip_effect(t){var r=$(this,lr).get(t);if(r){$(this,lr).delete(t);for(var n of r.d)wt(n,Pt),vr(n);for(n of r.m)wt(n,or),vr(n)}}process(t){var o;Ut=[],this.apply();var r=ya=[],n=[];for(const s of t)$t(this,qt,$n).call(this,s,r,n);if(ya=null,$t(this,qt,An).call(this)){$t(this,qt,Cn).call(this,n),$t(this,qt,Cn).call(this,r);for(const[s,l]of $(this,lr))Vo(s,l)}else{Ya=this,Me=null;for(const s of $(this,Cr))s(this);$(this,Cr).clear(),$(this,Wr)===0&&$t(this,qt,Mn).call(this),lo(n),lo(r),$(this,la).clear(),$(this,Mr).clear(),Ya=null,(o=$(this,ia))==null||o.resolve()}Nt=null}capture(t,r){r!==Mt&&!this.previous.has(t)&&this.previous.set(t,r),t.f&Or||(this.current.set(t,t.v),Nt==null||Nt.set(t,t.v))}activate(){Me=this,this.apply()}deactivate(){Me===this&&(Me=null,Nt=null)}flush(){var t;if(Ut.length>0)Me=this,Bo();else if($(this,Wr)===0&&!this.is_fork){for(const r of $(this,Cr))r(this);$(this,Cr).clear(),$t(this,qt,Mn).call(this),(t=$(this,ia))==null||t.resolve()}this.deactivate()}discard(){for(const t of $(this,oa))t(this);$(this,oa).clear()}increment(t){Fe(this,Wr,$(this,Wr)+1),t&&Fe(this,sa,$(this,sa)+1)}decrement(t){Fe(this,Wr,$(this,Wr)-1),t&&Fe(this,sa,$(this,sa)-1),!$(this,da)&&(Fe(this,da,!0),pr(()=>{Fe(this,da,!1),$t(this,qt,An).call(this)?Ut.length>0&&this.flush():this.revive()}))}revive(){for(const t of $(this,la))$(this,Mr).delete(t),wt(t,Pt),vr(t);for(const t of $(this,Mr))wt(t,or),vr(t);this.flush()}oncommit(t){$(this,Cr).add(t)}ondiscard(t){$(this,oa).add(t)}settled(){return($(this,ia)??Fe(this,ia,No())).promise}static ensure(){if(Me===null){const t=Me=new no;Wa.add(Me),Pa||pr(()=>{Me===t&&t.flush()})}return Me}apply(){}};Cr=new WeakMap,oa=new WeakMap,Wr=new WeakMap,sa=new WeakMap,ia=new WeakMap,la=new WeakMap,Mr=new WeakMap,lr=new WeakMap,da=new WeakMap,qt=new WeakSet,An=function(){return this.is_fork||$(this,sa)>0},$n=function(t,r,n){t.f^=Tt;for(var o=t.first;o!==null;){var s=o.f,l=(s&(sr|Zr))!==0,d=l&&(s&Tt)!==0,c=(s&jt)!==0,f=d||$(this,lr).has(o);if(!f&&o.fn!==null){l?c||(o.f^=Tt):s&_a?r.push(o):s&(ga|an)&&c?n.push(o):za(o)&&(ha(o),s&jr&&($(this,Mr).add(o),c&&wt(o,Pt)));var _=o.first;if(_!==null){o=_;continue}}for(;o!==null;){var w=o.next;if(w!==null){o=w;break}o=o.parent}}},Cn=function(t){for(var r=0;r<t.length;r+=1)zo(t[r],$(this,la),$(this,Mr))},Mn=function(){var s;if(Wa.size>1){this.previous.clear();var t=Me,r=Nt,n=!0;for(const l of Wa){if(l===this){n=!1;continue}const d=[];for(const[f,_]of this.current){if(l.current.has(f))if(n&&_!==l.current.get(f))l.current.set(f,_);else continue;d.push(f)}if(d.length===0)continue;const c=[...l.current.keys()].filter(f=>!this.current.has(f));if(c.length>0){var o=Ut;Ut=[];const f=new Set,_=new Map;for(const w of d)Wo(w,c,f,_);if(Ut.length>0){Me=l,l.apply();for(const w of Ut)$t(s=l,qt,$n).call(s,w,[],[]);l.deactivate()}Ut=o}}Me=t,Nt=r}$(this,lr).clear(),Wa.delete(this)};let Ir=no;function ki(e){var t=Pa;Pa=!0;try{for(var r;;){if(mi(),Ut.length===0&&(Me==null||Me.flush(),Ut.length===0))return nn=null,r;Bo()}}finally{Pa=t}}function Bo(){var e=null;try{for(var t=0;Ut.length>0;){var r=Ir.ensure();if(t++>1e3){var n,o;wi()}r.process(Ut),Lr.clear()}}finally{Ut=[],nn=null,ya=null}}function wi(){try{Xs()}catch(e){Tr(e,nn)}}let tr=null;function lo(e){var t=e.length;if(t!==0){for(var r=0;r<t;){var n=e[r++];if(!(n.f&(gr|jt))&&za(n)&&(tr=new Set,ha(n),n.deps===null&&n.first===null&&n.nodes===null&&n.teardown===null&&n.ac===null&&ls(n),(tr==null?void 0:tr.size)>0)){Lr.clear();for(const o of tr){if(o.f&(gr|jt))continue;const s=[o];let l=o.parent;for(;l!==null;)tr.has(l)&&(tr.delete(l),s.push(l)),l=l.parent;for(let d=s.length-1;d>=0;d--){const c=s[d];c.f&(gr|jt)||ha(c)}}tr.clear()}}tr=null}}function Wo(e,t,r,n){if(!r.has(e)&&(r.add(e),e.reactions!==null))for(const o of e.reactions){const s=o.f;s&Ft?Wo(o,t,r,n):s&(Wn|jr)&&!(s&Pt)&&qo(o,t,n)&&(wt(o,Pt),vr(o))}}function qo(e,t,r){const n=r.get(e);if(n!==void 0)return n;if(e.deps!==null)for(const o of e.deps){if(va.call(t,o))return!0;if(o.f&Ft&&qo(o,t,r))return r.set(o,!0),!0}return r.set(e,!1),!1}function vr(e){var t=nn=e,r=t.b;if(r!=null&&r.is_pending&&e.f&(_a|ga|an)&&!(e.f&xa)){r.defer_effect(e);return}for(;t.parent!==null;){t=t.parent;var n=t.f;if(ya!==null&&t===Je&&!(e.f&ga))return;if(n&(Zr|sr)){if(!(n&Tt))return;t.f^=Tt}}Ut.push(t)}function Vo(e,t){if(!(e.f&sr&&e.f&Tt)){e.f&Pt?t.d.push(e):e.f&or&&t.m.push(e),wt(e,Tt);for(var r=e.first;r!==null;)Vo(r,t),r=r.next}}function Si(e){let t=0,r=Xr(0),n;return()=>{Jn()&&(a(r),Yn(()=>(t===0&&(n=Sa(()=>e(()=>Oa(r)))),t+=1,()=>{pr(()=>{t-=1,t===0&&(n==null||n(),n=void 0,Oa(r))})})))}}var Ei=Sr|ka;function Ai(e,t,r,n){new $i(e,t,r,n)}var Yt,zn,dr,qr,Dt,cr,Gt,rr,mr,Vr,Nr,ca,ua,fa,_r,en,Ct,Ci,Mi,Ni,Nn,Ga,Ka,Tn;class $i{constructor(t,r,n,o){We(this,Ct);er(this,"parent");er(this,"is_pending",!1);er(this,"transform_error");We(this,Yt);We(this,zn,null);We(this,dr);We(this,qr);We(this,Dt);We(this,cr,null);We(this,Gt,null);We(this,rr,null);We(this,mr,null);We(this,Vr,0);We(this,Nr,0);We(this,ca,!1);We(this,ua,new Set);We(this,fa,new Set);We(this,_r,null);We(this,en,Si(()=>(Fe(this,_r,Xr($(this,Vr))),()=>{Fe(this,_r,null)})));var s;Fe(this,Yt,t),Fe(this,dr,r),Fe(this,qr,l=>{var d=Je;d.b=this,d.f|=Sn,n(l)}),this.parent=Je.b,this.transform_error=o??((s=this.parent)==null?void 0:s.transform_error)??(l=>l),Fe(this,Dt,wa(()=>{$t(this,Ct,Nn).call(this)},Ei))}defer_effect(t){zo(t,$(this,ua),$(this,fa))}is_rendered(){return!this.is_pending&&(!this.parent||this.parent.is_rendered())}has_pending_snippet(){return!!$(this,dr).pending}update_pending_count(t){$t(this,Ct,Tn).call(this,t),Fe(this,Vr,$(this,Vr)+t),!(!$(this,_r)||$(this,ca))&&(Fe(this,ca,!0),pr(()=>{Fe(this,ca,!1),$(this,_r)&&ba($(this,_r),$(this,Vr))}))}get_effect_pending(){return $(this,en).call(this),a($(this,_r))}error(t){var r=$(this,dr).onerror;let n=$(this,dr).failed;if(!r&&!n)throw t;$(this,cr)&&(Ot($(this,cr)),Fe(this,cr,null)),$(this,Gt)&&(Ot($(this,Gt)),Fe(this,Gt,null)),$(this,rr)&&(Ot($(this,rr)),Fe(this,rr,null));var o=!1,s=!1;const l=()=>{if(o){yi();return}o=!0,s&&ri(),$(this,rr)!==null&&Kr($(this,rr),()=>{Fe(this,rr,null)}),$t(this,Ct,Ka).call(this,()=>{Ir.ensure(),$t(this,Ct,Nn).call(this)})},d=c=>{try{s=!0,r==null||r(c,l),s=!1}catch(f){Tr(f,$(this,Dt)&&$(this,Dt).parent)}n&&Fe(this,rr,$t(this,Ct,Ka).call(this,()=>{Ir.ensure();try{return Bt(()=>{var f=Je;f.b=this,f.f|=Sn,n($(this,Yt),()=>c,()=>l)})}catch(f){return Tr(f,$(this,Dt).parent),null}}))};pr(()=>{var c;try{c=this.transform_error(t)}catch(f){Tr(f,$(this,Dt)&&$(this,Dt).parent);return}c!==null&&typeof c=="object"&&typeof c.then=="function"?c.then(d,f=>Tr(f,$(this,Dt)&&$(this,Dt).parent)):d(c)})}}Yt=new WeakMap,zn=new WeakMap,dr=new WeakMap,qr=new WeakMap,Dt=new WeakMap,cr=new WeakMap,Gt=new WeakMap,rr=new WeakMap,mr=new WeakMap,Vr=new WeakMap,Nr=new WeakMap,ca=new WeakMap,ua=new WeakMap,fa=new WeakMap,_r=new WeakMap,en=new WeakMap,Ct=new WeakSet,Ci=function(){try{Fe(this,cr,Bt(()=>$(this,qr).call(this,$(this,Yt))))}catch(t){this.error(t)}},Mi=function(t){const r=$(this,dr).failed;r&&Fe(this,rr,Bt(()=>{r($(this,Yt),()=>t,()=>()=>{})}))},Ni=function(){const t=$(this,dr).pending;t&&(this.is_pending=!0,Fe(this,Gt,Bt(()=>t($(this,Yt)))),pr(()=>{var r=Fe(this,mr,document.createDocumentFragment()),n=kr();r.append(n),Fe(this,cr,$t(this,Ct,Ka).call(this,()=>(Ir.ensure(),Bt(()=>$(this,qr).call(this,n))))),$(this,Nr)===0&&($(this,Yt).before(r),Fe(this,mr,null),Kr($(this,Gt),()=>{Fe(this,Gt,null)}),$t(this,Ct,Ga).call(this))}))},Nn=function(){try{if(this.is_pending=this.has_pending_snippet(),Fe(this,Nr,0),Fe(this,Vr,0),Fe(this,cr,Bt(()=>{$(this,qr).call(this,$(this,Yt))})),$(this,Nr)>0){var t=Fe(this,mr,document.createDocumentFragment());Zn($(this,cr),t);const r=$(this,dr).pending;Fe(this,Gt,Bt(()=>r($(this,Yt))))}else $t(this,Ct,Ga).call(this)}catch(r){this.error(r)}},Ga=function(){this.is_pending=!1;for(const t of $(this,ua))wt(t,Pt),vr(t);for(const t of $(this,fa))wt(t,or),vr(t);$(this,ua).clear(),$(this,fa).clear()},Ka=function(t){var r=Je,n=Ue,o=Wt;yr($(this,Dt)),Zt($(this,Dt)),pa($(this,Dt).ctx);try{return t()}catch(s){return Do(s),null}finally{yr(r),Zt(n),pa(o)}},Tn=function(t){var r;if(!this.has_pending_snippet()){this.parent&&$t(r=this.parent,Ct,Tn).call(r,t);return}Fe(this,Nr,$(this,Nr)+t),$(this,Nr)===0&&($t(this,Ct,Ga).call(this),$(this,Gt)&&Kr($(this,Gt),()=>{Fe(this,Gt,null)}),$(this,mr)&&($(this,Yt).before($(this,mr)),Fe(this,mr,null)))};function Go(e,t,r,n){const o=on;var s=e.filter(w=>!w.settled);if(r.length===0&&s.length===0){n(t.map(o));return}var l=Je,d=Ti(),c=s.length===1?s[0].promise:s.length>1?Promise.all(s.map(w=>w.promise)):null;function f(w){d();try{n(w)}catch(k){l.f&gr||Tr(k,l)}Pn()}if(r.length===0){c.then(()=>f(t.map(o)));return}function _(){d(),Promise.all(r.map(w=>Oi(w))).then(w=>f([...t.map(o),...w])).catch(w=>Tr(w,l))}c?c.then(_):_()}function Ti(){var e=Je,t=Ue,r=Wt,n=Me;return function(s=!0){yr(e),Zt(t),pa(r),s&&(n==null||n.activate())}}function Pn(e=!0){yr(null),Zt(null),pa(null),e&&(Me==null||Me.deactivate())}function Pi(){var e=Je.b,t=Me,r=e.is_rendered();return e.update_pending_count(1),t.increment(r),()=>{e.update_pending_count(-1),t.decrement(r)}}function on(e){var t=Ft|Pt,r=Ue!==null&&Ue.f&Ft?Ue:null;return Je!==null&&(Je.f|=ka),{ctx:Wt,deps:null,effects:null,equals:Lo,f:t,fn:e,reactions:null,rv:0,v:Mt,wv:0,parent:r??Je,ac:null}}function Oi(e,t,r){Je===null&&Vs();var o=void 0,s=Xr(Mt),l=!Ue,d=new Map;return Gi(()=>{var k;var c=No();o=c.promise;try{Promise.resolve(e()).then(c.resolve,c.reject).finally(Pn)}catch(I){c.reject(I),Pn()}var f=Me;if(l){var _=Pi();(k=d.get(f))==null||k.reject(Hr),d.delete(f),d.set(f,c)}const w=(I,T=void 0)=>{if(f.activate(),T)T!==Hr&&(s.f|=Or,ba(s,T));else{s.f&Or&&(s.f^=Or),ba(s,I);for(const[j,C]of d){if(d.delete(j),j===f)break;C.reject(Hr)}}_&&_()};c.promise.then(w,I=>w(null,I||"unknown"))}),ln(()=>{for(const c of d.values())c.reject(Hr)}),new Promise(c=>{function f(_){function w(){_===o?c(s):f(o)}_.then(w,w)}f(o)})}function ce(e){const t=on(e);return us(t),t}function Ko(e){const t=on(e);return t.equals=Ro,t}function Fi(e){var t=e.effects;if(t!==null){e.effects=null;for(var r=0;r<t.length;r+=1)Ot(t[r])}}function Ii(e){for(var t=e.parent;t!==null;){if(!(t.f&Ft))return t.f&gr?null:t;t=t.parent}return null}function Gn(e){var t,r=Je;yr(Ii(e));try{e.f&=~Yr,Fi(e),t=ps(e)}finally{yr(r)}return t}function Jo(e){var t=Gn(e);if(!e.equals(t)&&(e.wv=vs(),(!(Me!=null&&Me.is_fork)||e.deps===null)&&(e.v=t,e.deps===null))){wt(e,Tt);return}Rr||(Nt!==null?(Jn()||Me!=null&&Me.is_fork)&&Nt.set(e,t):Vn(e))}function Li(e){var t,r;if(e.effects!==null)for(const n of e.effects)(n.teardown||n.ac)&&((t=n.teardown)==null||t.call(n),(r=n.ac)==null||r.abort(Hr),n.teardown=Ce,n.ac=null,Ia(n,0),Xn(n))}function Yo(e){if(e.effects!==null)for(const t of e.effects)t.teardown&&ha(t)}let On=new Set;const Lr=new Map;let Xo=!1;function Xr(e,t){var r={f:0,v:e,reactions:null,equals:Lo,rv:0,wv:0};return r}function R(e,t){const r=Xr(e);return us(r),r}function Ri(e,t=!1,r=!0){const n=Xr(e);return t||(n.equals=Ro),n}function v(e,t,r=!1){Ue!==null&&(!nr||Ue.f&io)&&jo()&&Ue.f&(Ft|jr|Wn|io)&&(Qt===null||!va.call(Qt,e))&&ti();let n=r?mt(t):t;return ba(e,n)}function ba(e,t){if(!e.equals(t)){var r=e.v;Rr?Lr.set(e,t):Lr.set(e,r),e.v=t;var n=Ir.ensure();if(n.capture(e,r),e.f&Ft){const o=e;e.f&Pt&&Gn(o),Vn(o)}e.wv=vs(),Qo(e,Pt),Je!==null&&Je.f&Tt&&!(Je.f&(sr|Zr))&&(Jt===null?Ji([e]):Jt.push(e)),!n.is_fork&&On.size>0&&!Xo&&ji()}return t}function ji(){Xo=!1;for(const e of On)e.f&Tt&&wt(e,or),za(e)&&ha(e);On.clear()}function Oa(e){v(e,e.v+1)}function Qo(e,t){var r=e.reactions;if(r!==null)for(var n=r.length,o=0;o<n;o++){var s=r[o],l=s.f,d=(l&Pt)===0;if(d&&wt(s,t),l&Ft){var c=s;Nt==null||Nt.delete(c),l&Yr||(l&Xt&&(s.f|=Yr),Qo(c,or))}else d&&(l&jr&&tr!==null&&tr.add(s),vr(s))}}function mt(e){if(typeof e!="object"||e===null||Fr in e)return e;const t=Mo(e);if(t!==Ds&&t!==Us)return e;var r=new Map,n=Bn(e),o=R(0),s=Jr,l=d=>{if(Jr===s)return d();var c=Ue,f=Jr;Zt(null),go(s);var _=d();return Zt(c),go(f),_};return n&&r.set("length",R(e.length)),new Proxy(e,{defineProperty(d,c,f){(!("value"in f)||f.configurable===!1||f.enumerable===!1||f.writable===!1)&&Zs();var _=r.get(c);return _===void 0?l(()=>{var w=R(f.value);return r.set(c,w),w}):v(_,f.value,!0),!0},deleteProperty(d,c){var f=r.get(c);if(f===void 0){if(c in d){const _=l(()=>R(Mt));r.set(c,_),Oa(o)}}else v(f,Mt),Oa(o);return!0},get(d,c,f){var I;if(c===Fr)return e;var _=r.get(c),w=c in d;if(_===void 0&&(!w||(I=Pr(d,c))!=null&&I.writable)&&(_=l(()=>{var T=mt(w?d[c]:Mt),j=R(T);return j}),r.set(c,_)),_!==void 0){var k=a(_);return k===Mt?void 0:k}return Reflect.get(d,c,f)},getOwnPropertyDescriptor(d,c){var f=Reflect.getOwnPropertyDescriptor(d,c);if(f&&"value"in f){var _=r.get(c);_&&(f.value=a(_))}else if(f===void 0){var w=r.get(c),k=w==null?void 0:w.v;if(w!==void 0&&k!==Mt)return{enumerable:!0,configurable:!0,value:k,writable:!0}}return f},has(d,c){var k;if(c===Fr)return!0;var f=r.get(c),_=f!==void 0&&f.v!==Mt||Reflect.has(d,c);if(f!==void 0||Je!==null&&(!_||(k=Pr(d,c))!=null&&k.writable)){f===void 0&&(f=l(()=>{var I=_?mt(d[c]):Mt,T=R(I);return T}),r.set(c,f));var w=a(f);if(w===Mt)return!1}return _},set(d,c,f,_){var K;var w=r.get(c),k=c in d;if(n&&c==="length")for(var I=f;I<w.v;I+=1){var T=r.get(I+"");T!==void 0?v(T,Mt):I in d&&(T=l(()=>R(Mt)),r.set(I+"",T))}if(w===void 0)(!k||(K=Pr(d,c))!=null&&K.writable)&&(w=l(()=>R(void 0)),v(w,mt(f)),r.set(c,w));else{k=w.v!==Mt;var j=l(()=>mt(f));v(w,j)}var C=Reflect.getOwnPropertyDescriptor(d,c);if(C!=null&&C.set&&C.set.call(_,f),!k){if(n&&typeof c=="string"){var O=r.get("length"),q=Number(c);Number.isInteger(q)&&q>=O.v&&v(O,q+1)}Oa(o)}return!0},ownKeys(d){a(o);var c=Reflect.ownKeys(d).filter(w=>{var k=r.get(w);return k===void 0||k.v!==Mt});for(var[f,_]of r)_.v!==Mt&&!(f in d)&&c.push(f);return c},setPrototypeOf(){ei()}})}function co(e){try{if(e!==null&&typeof e=="object"&&Fr in e)return e[Fr]}catch{}return e}function Hi(e,t){return Object.is(co(e),co(t))}var uo,Zo,es,ts;function Di(){if(uo===void 0){uo=window,Zo=/Firefox/.test(navigator.userAgent);var e=Element.prototype,t=Node.prototype,r=Text.prototype;es=Pr(t,"firstChild").get,ts=Pr(t,"nextSibling").get,so(e)&&(e.__click=void 0,e.__className=void 0,e.__attributes=null,e.__style=void 0,e.__e=void 0),so(r)&&(r.__t=void 0)}}function kr(e=""){return document.createTextNode(e)}function wr(e){return es.call(e)}function Ua(e){return ts.call(e)}function i(e,t){return wr(e)}function xe(e,t=!1){{var r=wr(e);return r instanceof Comment&&r.data===""?Ua(r):r}}function p(e,t=1,r=!1){let n=e;for(;t--;)n=Ua(n);return n}function Ui(e){e.textContent=""}function rs(){return!1}function Kn(e,t,r){return document.createElementNS(t??Fo,e,void 0)}function zi(e,t){if(t){const r=document.body;e.autofocus=!0,pr(()=>{document.activeElement===r&&e.focus()})}}let fo=!1;function Bi(){fo||(fo=!0,document.addEventListener("reset",e=>{Promise.resolve().then(()=>{var t;if(!e.defaultPrevented)for(const r of e.target.elements)(t=r.__on_r)==null||t.call(r)})},{capture:!0}))}function sn(e){var t=Ue,r=Je;Zt(null),yr(null);try{return e()}finally{Zt(t),yr(r)}}function as(e,t,r,n=r){e.addEventListener(t,()=>sn(r));const o=e.__on_r;o?e.__on_r=()=>{o(),n(!0)}:e.__on_r=()=>n(!0),Bi()}function Wi(e){Je===null&&(Ue===null&&Ys(),Js()),Rr&&Ks()}function qi(e,t){var r=t.last;r===null?t.last=t.first=e:(r.next=e,e.prev=r,t.last=e)}function br(e,t){var r=Je;r!==null&&r.f&jt&&(e|=jt);var n={ctx:Wt,deps:null,nodes:null,f:e|Pt|Xt,first:null,fn:t,last:null,next:null,parent:r,b:r&&r.b,prev:null,teardown:null,wv:0,ac:null},o=n;if(e&_a)ya!==null?ya.push(n):vr(n);else if(t!==null){try{ha(n)}catch(l){throw Ot(n),l}o.deps===null&&o.teardown===null&&o.nodes===null&&o.first===o.last&&!(o.f&ka)&&(o=o.first,e&jr&&e&Sr&&o!==null&&(o.f|=Sr))}if(o!==null&&(o.parent=r,r!==null&&qi(o,r),Ue!==null&&Ue.f&Ft&&!(e&Zr))){var s=Ue;(s.effects??(s.effects=[])).push(o)}return n}function Jn(){return Ue!==null&&!nr}function ln(e){const t=br(ga,null);return wt(t,Tt),t.teardown=e,t}function Lt(e){Wi();var t=Je.f,r=!Ue&&(t&sr)!==0&&(t&xa)===0;if(r){var n=Wt;(n.e??(n.e=[])).push(e)}else return ns(e)}function ns(e){return br(_a|Ws,e)}function Vi(e){Ir.ensure();const t=br(Zr|ka,e);return(r={})=>new Promise(n=>{r.outro?Kr(t,()=>{Ot(t),n(void 0)}):(Ot(t),n(void 0))})}function dn(e){return br(_a,e)}function Gi(e){return br(Wn|ka,e)}function Yn(e,t=0){return br(ga|t,e)}function M(e,t=[],r=[],n=[]){Go(n,t,r,o=>{br(ga,()=>e(...o.map(a)))})}function wa(e,t=0){var r=br(jr|t,e);return r}function os(e,t=0){var r=br(an|t,e);return r}function Bt(e){return br(sr|ka,e)}function ss(e){var t=e.teardown;if(t!==null){const r=Rr,n=Ue;vo(!0),Zt(null);try{t.call(null)}finally{vo(r),Zt(n)}}}function Xn(e,t=!1){var r=e.first;for(e.first=e.last=null;r!==null;){const o=r.ac;o!==null&&sn(()=>{o.abort(Hr)});var n=r.next;r.f&Zr?r.parent=null:Ot(r,t),r=n}}function Ki(e){for(var t=e.first;t!==null;){var r=t.next;t.f&sr||Ot(t),t=r}}function Ot(e,t=!0){var r=!1;(t||e.f&Bs)&&e.nodes!==null&&e.nodes.end!==null&&(is(e.nodes.start,e.nodes.end),r=!0),Xn(e,t&&!r),Ia(e,0),wt(e,gr);var n=e.nodes&&e.nodes.t;if(n!==null)for(const s of n)s.stop();ss(e);var o=e.parent;o!==null&&o.first!==null&&ls(e),e.next=e.prev=e.teardown=e.ctx=e.deps=e.fn=e.nodes=e.ac=null}function is(e,t){for(;e!==null;){var r=e===t?null:Ua(e);e.remove(),e=r}}function ls(e){var t=e.parent,r=e.prev,n=e.next;r!==null&&(r.next=n),n!==null&&(n.prev=r),t!==null&&(t.first===e&&(t.first=n),t.last===e&&(t.last=r))}function Kr(e,t,r=!0){var n=[];ds(e,n,!0);var o=()=>{r&&Ot(e),t&&t()},s=n.length;if(s>0){var l=()=>--s||o();for(var d of n)d.out(l)}else o()}function ds(e,t,r){if(!(e.f&jt)){e.f^=jt;var n=e.nodes&&e.nodes.t;if(n!==null)for(const d of n)(d.is_global||r)&&t.push(d);for(var o=e.first;o!==null;){var s=o.next,l=(o.f&Sr)!==0||(o.f&sr)!==0&&(e.f&jr)!==0;ds(o,t,l?r:!1),o=s}}}function Qn(e){cs(e,!0)}function cs(e,t){if(e.f&jt){e.f^=jt;for(var r=e.first;r!==null;){var n=r.next,o=(r.f&Sr)!==0||(r.f&sr)!==0;cs(r,o?t:!1),r=n}var s=e.nodes&&e.nodes.t;if(s!==null)for(const l of s)(l.is_global||t)&&l.in()}}function Zn(e,t){if(e.nodes)for(var r=e.nodes.start,n=e.nodes.end;r!==null;){var o=r===n?null:Ua(r);t.append(r),r=o}}let Ja=!1,Rr=!1;function vo(e){Rr=e}let Ue=null,nr=!1;function Zt(e){Ue=e}let Je=null;function yr(e){Je=e}let Qt=null;function us(e){Ue!==null&&(Qt===null?Qt=[e]:Qt.push(e))}let zt=null,Vt=0,Jt=null;function Ji(e){Jt=e}let fs=1,Ur=0,Jr=Ur;function go(e){Jr=e}function vs(){return++fs}function za(e){var t=e.f;if(t&Pt)return!0;if(t&Ft&&(e.f&=~Yr),t&or){for(var r=e.deps,n=r.length,o=0;o<n;o++){var s=r[o];if(za(s)&&Jo(s),s.wv>e.wv)return!0}t&Xt&&Nt===null&&wt(e,Tt)}return!1}function gs(e,t,r=!0){var n=e.reactions;if(n!==null&&!(Qt!==null&&va.call(Qt,e)))for(var o=0;o<n.length;o++){var s=n[o];s.f&Ft?gs(s,t,!1):t===s&&(r?wt(s,Pt):s.f&Tt&&wt(s,or),vr(s))}}function ps(e){var j;var t=zt,r=Vt,n=Jt,o=Ue,s=Qt,l=Wt,d=nr,c=Jr,f=e.f;zt=null,Vt=0,Jt=null,Ue=f&(sr|Zr)?null:e,Qt=null,pa(e.ctx),nr=!1,Jr=++Ur,e.ac!==null&&(sn(()=>{e.ac.abort(Hr)}),e.ac=null);try{e.f|=En;var _=e.fn,w=_();e.f|=xa;var k=e.deps,I=Me==null?void 0:Me.is_fork;if(zt!==null){var T;if(I||Ia(e,Vt),k!==null&&Vt>0)for(k.length=Vt+zt.length,T=0;T<zt.length;T++)k[Vt+T]=zt[T];else e.deps=k=zt;if(Jn()&&e.f&Xt)for(T=Vt;T<k.length;T++)((j=k[T]).reactions??(j.reactions=[])).push(e)}else!I&&k!==null&&Vt<k.length&&(Ia(e,Vt),k.length=Vt);if(jo()&&Jt!==null&&!nr&&k!==null&&!(e.f&(Ft|or|Pt)))for(T=0;T<Jt.length;T++)gs(Jt[T],e);if(o!==null&&o!==e){if(Ur++,o.deps!==null)for(let C=0;C<r;C+=1)o.deps[C].rv=Ur;if(t!==null)for(const C of t)C.rv=Ur;Jt!==null&&(n===null?n=Jt:n.push(...Jt))}return e.f&Or&&(e.f^=Or),w}catch(C){return Do(C)}finally{e.f^=En,zt=t,Vt=r,Jt=n,Ue=o,Qt=s,pa(l),nr=d,Jr=c}}function Yi(e,t){let r=t.reactions;if(r!==null){var n=Rs.call(r,e);if(n!==-1){var o=r.length-1;o===0?r=t.reactions=null:(r[n]=r[o],r.pop())}}if(r===null&&t.f&Ft&&(zt===null||!va.call(zt,t))){var s=t;s.f&Xt&&(s.f^=Xt,s.f&=~Yr),Vn(s),Li(s),Ia(s,0)}}function Ia(e,t){var r=e.deps;if(r!==null)for(var n=t;n<r.length;n++)Yi(e,r[n])}function ha(e){var t=e.f;if(!(t&gr)){wt(e,Tt);var r=Je,n=Ja;Je=e,Ja=!0;try{t&(jr|an)?Ki(e):Xn(e),ss(e);var o=ps(e);e.teardown=typeof o=="function"?o:null,e.wv=fs;var s;wn&&hi&&e.f&Pt&&e.deps}finally{Ja=n,Je=r}}}async function ys(){await Promise.resolve(),ki()}function a(e){var t=e.f,r=(t&Ft)!==0;if(Ue!==null&&!nr){var n=Je!==null&&(Je.f&gr)!==0;if(!n&&(Qt===null||!va.call(Qt,e))){var o=Ue.deps;if(Ue.f&En)e.rv<Ur&&(e.rv=Ur,zt===null&&o!==null&&o[Vt]===e?Vt++:zt===null?zt=[e]:zt.push(e));else{(Ue.deps??(Ue.deps=[])).push(e);var s=e.reactions;s===null?e.reactions=[Ue]:va.call(s,Ue)||s.push(Ue)}}}if(Rr&&Lr.has(e))return Lr.get(e);if(r){var l=e;if(Rr){var d=l.v;return(!(l.f&Tt)&&l.reactions!==null||hs(l))&&(d=Gn(l)),Lr.set(l,d),d}var c=(l.f&Xt)===0&&!nr&&Ue!==null&&(Ja||(Ue.f&Xt)!==0),f=(l.f&xa)===0;za(l)&&(c&&(l.f|=Xt),Jo(l)),c&&!f&&(Yo(l),bs(l))}if(Nt!=null&&Nt.has(e))return Nt.get(e);if(e.f&Or)throw e.v;return e.v}function bs(e){if(e.f|=Xt,e.deps!==null)for(const t of e.deps)(t.reactions??(t.reactions=[])).push(e),t.f&Ft&&!(t.f&Xt)&&(Yo(t),bs(t))}function hs(e){if(e.v===Mt)return!0;if(e.deps===null)return!1;for(const t of e.deps)if(Lr.has(t)||t.f&Ft&&hs(t))return!0;return!1}function Sa(e){var t=nr;try{return nr=!0,e()}finally{nr=t}}function Xi(e){return e.endsWith("capture")&&e!=="gotpointercapture"&&e!=="lostpointercapture"}const Qi=["beforeinput","click","change","dblclick","contextmenu","focusin","focusout","input","keydown","keyup","mousedown","mousemove","mouseout","mouseover","mouseup","pointerdown","pointermove","pointerout","pointerover","pointerup","touchend","touchmove","touchstart"];function Zi(e){return Qi.includes(e)}const el={formnovalidate:"formNoValidate",ismap:"isMap",nomodule:"noModule",playsinline:"playsInline",readonly:"readOnly",defaultvalue:"defaultValue",defaultchecked:"defaultChecked",srcobject:"srcObject",novalidate:"noValidate",allowfullscreen:"allowFullscreen",disablepictureinpicture:"disablePictureInPicture",disableremoteplayback:"disableRemotePlayback"};function tl(e){return e=e.toLowerCase(),el[e]??e}const rl=["touchstart","touchmove"];function al(e){return rl.includes(e)}const zr=Symbol("events"),ms=new Set,Fn=new Set;function _s(e,t,r,n={}){function o(s){if(n.capture||In.call(t,s),!s.cancelBubble)return sn(()=>r==null?void 0:r.call(this,s))}return e.startsWith("pointer")||e.startsWith("touch")||e==="wheel"?pr(()=>{t.addEventListener(e,o,n)}):t.addEventListener(e,o,n),o}function xr(e,t,r,n,o){var s={capture:n,passive:o},l=_s(e,t,r,s);(t===document.body||t===window||t===document||t instanceof HTMLMediaElement)&&ln(()=>{t.removeEventListener(e,l,s)})}function te(e,t,r){(t[zr]??(t[zr]={}))[e]=r}function ir(e){for(var t=0;t<e.length;t++)ms.add(e[t]);for(var r of Fn)r(e)}let po=null;function In(e){var C,O;var t=this,r=t.ownerDocument,n=e.type,o=((C=e.composedPath)==null?void 0:C.call(e))||[],s=o[0]||e.target;po=e;var l=0,d=po===e&&e[zr];if(d){var c=o.indexOf(d);if(c!==-1&&(t===document||t===window)){e[zr]=t;return}var f=o.indexOf(t);if(f===-1)return;c<=f&&(l=c)}if(s=o[l]||e.target,s!==t){js(e,"currentTarget",{configurable:!0,get(){return s||r}});var _=Ue,w=Je;Zt(null),yr(null);try{for(var k,I=[];s!==null;){var T=s.assignedSlot||s.parentNode||s.host||null;try{var j=(O=s[zr])==null?void 0:O[n];j!=null&&(!s.disabled||e.target===s)&&j.call(s,e)}catch(q){k?I.push(q):k=q}if(e.cancelBubble||T===t||T===null)break;s=T}if(k){for(let q of I)queueMicrotask(()=>{throw q});throw k}}finally{e[zr]=t,delete e.currentTarget,Zt(_),yr(w)}}}var $o;const pn=(($o=globalThis==null?void 0:globalThis.window)==null?void 0:$o.trustedTypes)&&globalThis.window.trustedTypes.createPolicy("svelte-trusted-html",{createHTML:e=>e});function nl(e){return(pn==null?void 0:pn.createHTML(e))??e}function xs(e){var t=Kn("template");return t.innerHTML=nl(e.replaceAll("<!>","<!---->")),t.content}function ma(e,t){var r=Je;r.nodes===null&&(r.nodes={start:e,end:t,a:null,t:null})}function x(e,t){var r=(t&ui)!==0,n=(t&fi)!==0,o,s=!e.startsWith("<!>");return()=>{o===void 0&&(o=xs(s?e:"<!>"+e),r||(o=wr(o)));var l=n||Zo?document.importNode(o,!0):o.cloneNode(!0);if(r){var d=wr(l),c=l.lastChild;ma(d,c)}else ma(l,l);return l}}function ol(e,t,r="svg"){var n=!e.startsWith("<!>"),o=`<${r}>${n?e:"<!>"+e}</${r}>`,s;return()=>{if(!s){var l=xs(o),d=wr(l);s=wr(d)}var c=s.cloneNode(!0);return ma(c,c),c}}function sl(e,t){return ol(e,t,"svg")}function Re(){var e=document.createDocumentFragment(),t=document.createComment(""),r=kr();return e.append(t,r),ma(t,r),e}function g(e,t){e!==null&&e.before(t)}function y(e,t){var r=t==null?"":typeof t=="object"?`${t}`:t;r!==(e.__t??(e.__t=e.nodeValue))&&(e.__t=r,e.nodeValue=`${r}`)}function il(e,t){return ll(e,t)}const qa=new Map;function ll(e,{target:t,anchor:r,props:n={},events:o,context:s,intro:l=!0,transformError:d}){Di();var c=void 0,f=Vi(()=>{var _=r??t.appendChild(kr());Ai(_,{pending:()=>{}},I=>{Te({});var T=Wt;s&&(T.c=s),o&&(n.$$events=o),c=e(I,n)||{},Pe()},d);var w=new Set,k=I=>{for(var T=0;T<I.length;T++){var j=I[T];if(!w.has(j)){w.add(j);var C=al(j);for(const K of[t,document]){var O=qa.get(K);O===void 0&&(O=new Map,qa.set(K,O));var q=O.get(j);q===void 0?(K.addEventListener(j,In,{passive:C}),O.set(j,1)):O.set(j,q+1)}}}};return k(rn(ms)),Fn.add(k),()=>{var C;for(var I of w)for(const O of[t,document]){var T=qa.get(O),j=T.get(I);--j==0?(O.removeEventListener(I,In),T.delete(I),T.size===0&&qa.delete(O)):T.set(I,j)}Fn.delete(k),_!==r&&((C=_.parentNode)==null||C.removeChild(_))}});return dl.set(c,f),c}let dl=new WeakMap;var ar,ur,Kt,Gr,Ha,Da,tn;class cn{constructor(t,r=!0){er(this,"anchor");We(this,ar,new Map);We(this,ur,new Map);We(this,Kt,new Map);We(this,Gr,new Set);We(this,Ha,!0);We(this,Da,t=>{if($(this,ar).has(t)){var r=$(this,ar).get(t),n=$(this,ur).get(r);if(n)Qn(n),$(this,Gr).delete(r);else{var o=$(this,Kt).get(r);o&&!(o.effect.f&jt)&&($(this,ur).set(r,o.effect),$(this,Kt).delete(r),o.fragment.lastChild.remove(),this.anchor.before(o.fragment),n=o.effect)}for(const[s,l]of $(this,ar)){if($(this,ar).delete(s),s===t)break;const d=$(this,Kt).get(l);d&&(Ot(d.effect),$(this,Kt).delete(l))}for(const[s,l]of $(this,ur)){if(s===r||$(this,Gr).has(s)||l.f&jt)continue;const d=()=>{if(Array.from($(this,ar).values()).includes(s)){var f=document.createDocumentFragment();Zn(l,f),f.append(kr()),$(this,Kt).set(s,{effect:l,fragment:f})}else Ot(l);$(this,Gr).delete(s),$(this,ur).delete(s)};$(this,Ha)||!n?($(this,Gr).add(s),Kr(l,d,!1)):d()}}});We(this,tn,t=>{$(this,ar).delete(t);const r=Array.from($(this,ar).values());for(const[n,o]of $(this,Kt))r.includes(n)||(Ot(o.effect),$(this,Kt).delete(n))});this.anchor=t,Fe(this,Ha,r)}ensure(t,r){var n=Me,o=rs();if(r&&!$(this,ur).has(t)&&!$(this,Kt).has(t))if(o){var s=document.createDocumentFragment(),l=kr();s.append(l),$(this,Kt).set(t,{effect:Bt(()=>r(l)),fragment:s})}else $(this,ur).set(t,Bt(()=>r(this.anchor)));if($(this,ar).set(n,t),o){for(const[d,c]of $(this,ur))d===t?n.unskip_effect(c):n.skip_effect(c);for(const[d,c]of $(this,Kt))d===t?n.unskip_effect(c.effect):n.skip_effect(c.effect);n.oncommit($(this,Da)),n.ondiscard($(this,tn))}else $(this,Da).call(this,n)}}ar=new WeakMap,ur=new WeakMap,Kt=new WeakMap,Gr=new WeakMap,Ha=new WeakMap,Da=new WeakMap,tn=new WeakMap;function z(e,t,r=!1){var n=new cn(e),o=r?Sr:0;function s(l,d){n.ensure(l,d)}wa(()=>{var l=!1;t((d,c=0)=>{l=!0,s(c,d)}),l||s(-1,null)},o)}function rt(e,t){return t}function cl(e,t,r){for(var n=[],o=t.length,s,l=t.length,d=0;d<o;d++){let w=t[d];Kr(w,()=>{if(s){if(s.pending.delete(w),s.done.add(w),s.pending.size===0){var k=e.outrogroups;Ln(e,rn(s.done)),k.delete(s),k.size===0&&(e.outrogroups=null)}}else l-=1},!1)}if(l===0){var c=n.length===0&&r!==null;if(c){var f=r,_=f.parentNode;Ui(_),_.append(f),e.items.clear()}Ln(e,t,!c)}else s={pending:new Set(t),done:new Set},(e.outrogroups??(e.outrogroups=new Set)).add(s)}function Ln(e,t,r=!0){var n;if(e.pending.size>0){n=new Set;for(const l of e.pending.values())for(const d of l)n.add(e.items.get(d).e)}for(var o=0;o<t.length;o++){var s=t[o];if(n!=null&&n.has(s)){s.f|=fr;const l=document.createDocumentFragment();Zn(s,l)}else Ot(t[o],r)}}var yo;function Xe(e,t,r,n,o,s=null){var l=e,d=new Map,c=(t&Oo)!==0;if(c){var f=e;l=f.appendChild(kr())}var _=null,w=Ko(()=>{var K=r();return Bn(K)?K:K==null?[]:rn(K)}),k,I=new Map,T=!0;function j(K){q.effect.f&gr||(q.pending.delete(K),q.fallback=_,ul(q,k,l,t,n),_!==null&&(k.length===0?_.f&fr?(_.f^=fr,Ta(_,null,l)):Qn(_):Kr(_,()=>{_=null})))}function C(K){q.pending.delete(K)}var O=wa(()=>{k=a(w);for(var K=k.length,S=new Set,m=Me,N=rs(),P=0;P<K;P+=1){var W=k[P],Se=n(W,P),_e=T?null:d.get(Se);_e?(_e.v&&ba(_e.v,W),_e.i&&ba(_e.i,P),N&&m.unskip_effect(_e.e)):(_e=fl(d,T?l:yo??(yo=kr()),W,Se,P,o,t,r),T||(_e.e.f|=fr),d.set(Se,_e)),S.add(Se)}if(K===0&&s&&!_&&(T?_=Bt(()=>s(l)):(_=Bt(()=>s(yo??(yo=kr()))),_.f|=fr)),K>S.size&&Gs(),!T)if(I.set(m,S),N){for(const[je,qe]of d)S.has(je)||m.skip_effect(qe.e);m.oncommit(j),m.ondiscard(C)}else j(m);a(w)}),q={effect:O,items:d,pending:I,outrogroups:null,fallback:_};T=!1}function $a(e){for(;e!==null&&!(e.f&sr);)e=e.next;return e}function ul(e,t,r,n,o){var _e,je,qe,B,Z,ee,oe,Oe,ze;var s=(n&oi)!==0,l=t.length,d=e.items,c=$a(e.effect.first),f,_=null,w,k=[],I=[],T,j,C,O;if(s)for(O=0;O<l;O+=1)T=t[O],j=o(T,O),C=d.get(j).e,C.f&fr||((je=(_e=C.nodes)==null?void 0:_e.a)==null||je.measure(),(w??(w=new Set)).add(C));for(O=0;O<l;O+=1){if(T=t[O],j=o(T,O),C=d.get(j).e,e.outrogroups!==null)for(const ke of e.outrogroups)ke.pending.delete(C),ke.done.delete(C);if(C.f&fr)if(C.f^=fr,C===c)Ta(C,null,r);else{var q=_?_.next:c;C===e.effect.last&&(e.effect.last=C.prev),C.prev&&(C.prev.next=C.next),C.next&&(C.next.prev=C.prev),Ar(e,_,C),Ar(e,C,q),Ta(C,q,r),_=C,k=[],I=[],c=$a(_.next);continue}if(C.f&jt&&(Qn(C),s&&((B=(qe=C.nodes)==null?void 0:qe.a)==null||B.unfix(),(w??(w=new Set)).delete(C))),C!==c){if(f!==void 0&&f.has(C)){if(k.length<I.length){var K=I[0],S;_=K.prev;var m=k[0],N=k[k.length-1];for(S=0;S<k.length;S+=1)Ta(k[S],K,r);for(S=0;S<I.length;S+=1)f.delete(I[S]);Ar(e,m.prev,N.next),Ar(e,_,m),Ar(e,N,K),c=K,_=N,O-=1,k=[],I=[]}else f.delete(C),Ta(C,c,r),Ar(e,C.prev,C.next),Ar(e,C,_===null?e.effect.first:_.next),Ar(e,_,C),_=C;continue}for(k=[],I=[];c!==null&&c!==C;)(f??(f=new Set)).add(c),I.push(c),c=$a(c.next);if(c===null)continue}C.f&fr||k.push(C),_=C,c=$a(C.next)}if(e.outrogroups!==null){for(const ke of e.outrogroups)ke.pending.size===0&&(Ln(e,rn(ke.done)),(Z=e.outrogroups)==null||Z.delete(ke));e.outrogroups.size===0&&(e.outrogroups=null)}if(c!==null||f!==void 0){var P=[];if(f!==void 0)for(C of f)C.f&jt||P.push(C);for(;c!==null;)!(c.f&jt)&&c!==e.fallback&&P.push(c),c=$a(c.next);var W=P.length;if(W>0){var Se=n&Oo&&l===0?r:null;if(s){for(O=0;O<W;O+=1)(oe=(ee=P[O].nodes)==null?void 0:ee.a)==null||oe.measure();for(O=0;O<W;O+=1)(ze=(Oe=P[O].nodes)==null?void 0:Oe.a)==null||ze.fix()}cl(e,P,Se)}}s&&pr(()=>{var ke,He;if(w!==void 0)for(C of w)(He=(ke=C.nodes)==null?void 0:ke.a)==null||He.apply()})}function fl(e,t,r,n,o,s,l,d){var c=l&ai?l&si?Xr(r):Ri(r,!1,!1):null,f=l&ni?Xr(o):null;return{v:c,i:f,e:Bt(()=>(s(t,c??r,f??o,d),()=>{e.delete(n)}))}}function Ta(e,t,r){if(e.nodes)for(var n=e.nodes.start,o=e.nodes.end,s=t&&!(t.f&fr)?t.nodes.start:r;n!==null;){var l=Ua(n);if(s.before(n),n===o)return;n=l}}function Ar(e,t,r){t===null?e.effect.first=r:t.next=r,r===null?e.effect.last=t:r.prev=t}function vl(e,t,r=!1,n=!1,o=!1){var s=e,l="";M(()=>{var d=Je;if(l!==(l=t()??"")&&(d.nodes!==null&&(is(d.nodes.start,d.nodes.end),d.nodes=null),l!=="")){var c=r?Io:n?vi:void 0,f=Kn(r?"svg":n?"math":"template",c);f.innerHTML=l;var _=r||n?f:f.content;if(ma(wr(_),_.lastChild),r||n)for(;wr(_);)s.before(wr(_));else s.before(_)}})}function ct(e,t,...r){var n=new cn(e);wa(()=>{const o=t()??null;n.ensure(o,o&&(s=>o(s,...r)))},Sr)}function gl(e,t,r){var n=new cn(e);wa(()=>{var o=t()??null;n.ensure(o,o&&(s=>r(s,o)))},Sr)}function pl(e,t,r,n,o,s){var l=null,d=e,c=new cn(d,!1);wa(()=>{const f=t()||null;var _=Io;if(f===null){c.ensure(null,null);return}return c.ensure(f,w=>{if(f){if(l=Kn(f,_),ma(l,l),n){var k=l.appendChild(kr());n(l,k)}Je.nodes.end=l,w.before(l)}}),()=>{}},Sr),ln(()=>{})}function yl(e,t){var r=void 0,n;os(()=>{r!==(r=t())&&(n&&(Ot(n),n=null),r&&(n=Bt(()=>{dn(()=>r(e))})))})}function ks(e){var t,r,n="";if(typeof e=="string"||typeof e=="number")n+=e;else if(typeof e=="object")if(Array.isArray(e)){var o=e.length;for(t=0;t<o;t++)e[t]&&(r=ks(e[t]))&&(n&&(n+=" "),n+=r)}else for(r in e)e[r]&&(n&&(n+=" "),n+=r);return n}function bl(){for(var e,t,r=0,n="",o=arguments.length;r<o;r++)(e=arguments[r])&&(t=ks(e))&&(n&&(n+=" "),n+=t);return n}function eo(e){return typeof e=="object"?bl(e):e??""}const bo=[...` 	
\r\f \v\uFEFF`];function hl(e,t,r){var n=e==null?"":""+e;if(r){for(var o of Object.keys(r))if(r[o])n=n?n+" "+o:o;else if(n.length)for(var s=o.length,l=0;(l=n.indexOf(o,l))>=0;){var d=l+s;(l===0||bo.includes(n[l-1]))&&(d===n.length||bo.includes(n[d]))?n=(l===0?"":n.substring(0,l))+n.substring(d+1):l=d}}return n===""?null:n}function ho(e,t=!1){var r=t?" !important;":";",n="";for(var o of Object.keys(e)){var s=e[o];s!=null&&s!==""&&(n+=" "+o+": "+s+r)}return n}function yn(e){return e[0]!=="-"||e[1]!=="-"?e.toLowerCase():e}function ml(e,t){if(t){var r="",n,o;if(Array.isArray(t)?(n=t[0],o=t[1]):n=t,e){e=String(e).replaceAll(/\s*\/\*.*?\*\/\s*/g,"").trim();var s=!1,l=0,d=!1,c=[];n&&c.push(...Object.keys(n).map(yn)),o&&c.push(...Object.keys(o).map(yn));var f=0,_=-1;const j=e.length;for(var w=0;w<j;w++){var k=e[w];if(d?k==="/"&&e[w-1]==="*"&&(d=!1):s?s===k&&(s=!1):k==="/"&&e[w+1]==="*"?d=!0:k==='"'||k==="'"?s=k:k==="("?l++:k===")"&&l--,!d&&s===!1&&l===0){if(k===":"&&_===-1)_=w;else if(k===";"||w===j-1){if(_!==-1){var I=yn(e.substring(f,_).trim());if(!c.includes(I)){k!==";"&&w++;var T=e.substring(f,w).trim();r+=" "+T+";"}}f=w+1,_=-1}}}}return n&&(r+=ho(n)),o&&(r+=ho(o,!0)),r=r.trim(),r===""?null:r}return e==null?null:String(e)}function Qe(e,t,r,n,o,s){var l=e.__className;if(l!==r||l===void 0){var d=hl(r,n,s);d==null?e.removeAttribute("class"):t?e.className=d:e.setAttribute("class",d),e.__className=r}else if(s&&o!==s)for(var c in s){var f=!!s[c];(o==null||f!==!!o[c])&&e.classList.toggle(c,f)}return s}function bn(e,t={},r,n){for(var o in r){var s=r[o];t[o]!==s&&(r[o]==null?e.style.removeProperty(o):e.style.setProperty(o,s,n))}}function _l(e,t,r,n){var o=e.__style;if(o!==t){var s=ml(t,n);s==null?e.removeAttribute("style"):e.style.cssText=s,e.__style=t}else n&&(Array.isArray(n)?(bn(e,r==null?void 0:r[0],n[0]),bn(e,r==null?void 0:r[1],n[1],"important")):bn(e,r,n));return n}function La(e,t,r=!1){if(e.multiple){if(t==null)return;if(!Bn(t))return pi();for(var n of e.options)n.selected=t.includes(Fa(n));return}for(n of e.options){var o=Fa(n);if(Hi(o,t)){n.selected=!0;return}}(!r||t!==void 0)&&(e.selectedIndex=-1)}function to(e){var t=new MutationObserver(()=>{La(e,e.__value)});t.observe(e,{childList:!0,subtree:!0,attributes:!0,attributeFilter:["value"]}),ln(()=>{t.disconnect()})}function Rn(e,t,r=t){var n=new WeakSet,o=!0;as(e,"change",s=>{var l=s?"[selected]":":checked",d;if(e.multiple)d=[].map.call(e.querySelectorAll(l),Fa);else{var c=e.querySelector(l)??e.querySelector("option:not([disabled])");d=c&&Fa(c)}r(d),Me!==null&&n.add(Me)}),dn(()=>{var s=t();if(e===document.activeElement){var l=Ya??Me;if(n.has(l))return}if(La(e,s,o),o&&s===void 0){var d=e.querySelector(":checked");d!==null&&(s=Fa(d),r(s))}e.__value=s,o=!1}),to(e)}function Fa(e){return"__value"in e?e.__value:e.value}const Ca=Symbol("class"),Ma=Symbol("style"),ws=Symbol("is custom element"),Ss=Symbol("is html"),xl=qn?"option":"OPTION",kl=qn?"select":"SELECT",wl=qn?"progress":"PROGRESS";function hr(e,t){var r=ro(e);r.value===(r.value=t??void 0)||e.value===t&&(t!==0||e.nodeName!==wl)||(e.value=t??"")}function Sl(e,t){t?e.hasAttribute("selected")||e.setAttribute("selected",""):e.removeAttribute("selected")}function $e(e,t,r,n){var o=ro(e);o[t]!==(o[t]=r)&&(t==="loading"&&(e[qs]=r),r==null?e.removeAttribute(t):typeof r!="string"&&Es(e).includes(t)?e[t]=r:e.setAttribute(t,r))}function El(e,t,r,n,o=!1,s=!1){var l=ro(e),d=l[ws],c=!l[Ss],f=t||{},_=e.nodeName===xl;for(var w in t)w in r||(r[w]=null);r.class?r.class=eo(r.class):r[Ca]&&(r.class=null),r[Ma]&&(r.style??(r.style=null));var k=Es(e);for(const S in r){let m=r[S];if(_&&S==="value"&&m==null){e.value=e.__value="",f[S]=m;continue}if(S==="class"){var I=e.namespaceURI==="http://www.w3.org/1999/xhtml";Qe(e,I,m,n,t==null?void 0:t[Ca],r[Ca]),f[S]=m,f[Ca]=r[Ca];continue}if(S==="style"){_l(e,m,t==null?void 0:t[Ma],r[Ma]),f[S]=m,f[Ma]=r[Ma];continue}var T=f[S];if(!(m===T&&!(m===void 0&&e.hasAttribute(S)))){f[S]=m;var j=S[0]+S[1];if(j!=="$$")if(j==="on"){const N={},P="$$"+S;let W=S.slice(2);var C=Zi(W);if(Xi(W)&&(W=W.slice(0,-7),N.capture=!0),!C&&T){if(m!=null)continue;e.removeEventListener(W,f[P],N),f[P]=null}if(C)te(W,e,m),ir([W]);else if(m!=null){let Se=function(_e){f[S].call(this,_e)};var K=Se;f[P]=_s(W,e,Se,N)}}else if(S==="style")$e(e,S,m);else if(S==="autofocus")zi(e,!!m);else if(!d&&(S==="__value"||S==="value"&&m!=null))e.value=e.__value=m;else if(S==="selected"&&_)Sl(e,m);else{var O=S;c||(O=tl(O));var q=O==="defaultValue"||O==="defaultChecked";if(m==null&&!d&&!q)if(l[S]=null,O==="value"||O==="checked"){let N=e;const P=t===void 0;if(O==="value"){let W=N.defaultValue;N.removeAttribute(O),N.defaultValue=W,N.value=N.__value=P?W:null}else{let W=N.defaultChecked;N.removeAttribute(O),N.defaultChecked=W,N.checked=P?W:!1}}else e.removeAttribute(S);else q||k.includes(O)&&(d||typeof m!="string")?(e[O]=m,O in l&&(l[O]=Mt)):typeof m!="function"&&$e(e,O,m)}}}return f}function mo(e,t,r=[],n=[],o=[],s,l=!1,d=!1){Go(o,r,n,c=>{var f=void 0,_={},w=e.nodeName===kl,k=!1;if(os(()=>{var T=t(...c.map(a)),j=El(e,f,T,s,l,d);k&&w&&"value"in T&&La(e,T.value);for(let O of Object.getOwnPropertySymbols(_))T[O]||Ot(_[O]);for(let O of Object.getOwnPropertySymbols(T)){var C=T[O];O.description===gi&&(!f||C!==f[O])&&(_[O]&&Ot(_[O]),_[O]=Bt(()=>yl(e,()=>C))),j[O]=C}f=j}),w){var I=e;dn(()=>{La(I,f.value,!0),to(I)})}k=!0})}function ro(e){return e.__attributes??(e.__attributes={[ws]:e.nodeName.includes("-"),[Ss]:e.namespaceURI===Fo})}var _o=new Map;function Es(e){var t=e.getAttribute("is")||e.nodeName,r=_o.get(t);if(r)return r;_o.set(t,r=[]);for(var n,o=e,s=Element.prototype;s!==o;){n=Hs(o);for(var l in n)n[l].set&&r.push(l);o=Mo(o)}return r}function Br(e,t,r=t){var n=new WeakSet;as(e,"input",async o=>{var s=o?e.defaultValue:e.value;if(s=hn(e)?mn(s):s,r(s),Me!==null&&n.add(Me),await ys(),s!==(s=t())){var l=e.selectionStart,d=e.selectionEnd,c=e.value.length;if(e.value=s??"",d!==null){var f=e.value.length;l===d&&d===c&&f>c?(e.selectionStart=f,e.selectionEnd=f):(e.selectionStart=l,e.selectionEnd=Math.min(d,f))}}}),Sa(t)==null&&e.value&&(r(hn(e)?mn(e.value):e.value),Me!==null&&n.add(Me)),Yn(()=>{var o=t();if(e===document.activeElement){var s=Ya??Me;if(n.has(s))return}hn(e)&&o===mn(e.value)||e.type==="date"&&!o&&!e.value||o!==e.value&&(e.value=o??"")})}function hn(e){var t=e.type;return t==="number"||t==="range"}function mn(e){return e===""?null:+e}function xo(e,t){return e===t||(e==null?void 0:e[Fr])===t}function jn(e={},t,r,n){return dn(()=>{var o,s;return Yn(()=>{o=s,s=[],Sa(()=>{e!==r(...s)&&(t(e,...s),o&&xo(r(...o),e)&&t(null,...o))})}),()=>{pr(()=>{s&&xo(r(...s),e)&&t(null,...s)})}}),e}let Va=!1;function Al(e){var t=Va;try{return Va=!1,[e(),Va]}finally{Va=t}}const $l={get(e,t){if(!e.exclude.includes(t))return e.props[t]},set(e,t){return!1},getOwnPropertyDescriptor(e,t){if(!e.exclude.includes(t)&&t in e.props)return{enumerable:!0,configurable:!0,value:e.props[t]}},has(e,t){return e.exclude.includes(t)?!1:t in e.props},ownKeys(e){return Reflect.ownKeys(e.props).filter(t=>!e.exclude.includes(t))}};function ut(e,t,r){return new Proxy({props:e,exclude:t},$l)}const Cl={get(e,t){let r=e.props.length;for(;r--;){let n=e.props[r];if(Aa(n)&&(n=n()),typeof n=="object"&&n!==null&&t in n)return n[t]}},set(e,t,r){let n=e.props.length;for(;n--;){let o=e.props[n];Aa(o)&&(o=o());const s=Pr(o,t);if(s&&s.set)return s.set(r),!0}return!1},getOwnPropertyDescriptor(e,t){let r=e.props.length;for(;r--;){let n=e.props[r];if(Aa(n)&&(n=n()),typeof n=="object"&&n!==null&&t in n){const o=Pr(n,t);return o&&!o.configurable&&(o.configurable=!0),o}}},has(e,t){if(t===Fr||t===To)return!1;for(let r of e.props)if(Aa(r)&&(r=r()),r!=null&&t in r)return!0;return!1},ownKeys(e){const t=[];for(let r of e.props)if(Aa(r)&&(r=r()),!!r){for(const n in r)t.includes(n)||t.push(n);for(const n of Object.getOwnPropertySymbols(r))t.includes(n)||t.push(n)}return t}};function gt(...e){return new Proxy({props:e},Cl)}function aa(e,t,r,n){var q;var o=(r&di)!==0,s=(r&ci)!==0,l=n,d=!0,c=()=>(d&&(d=!1,l=s?Sa(n):n),l),f;if(o){var _=Fr in e||To in e;f=((q=Pr(e,t))==null?void 0:q.set)??(_&&t in e?K=>e[t]=K:void 0)}var w,k=!1;o?[w,k]=Al(()=>e[t]):w=e[t],w===void 0&&n!==void 0&&(w=c(),f&&(Qs(),f(w)));var I;if(I=()=>{var K=e[t];return K===void 0?c():(d=!0,K)},!(r&li))return I;if(f){var T=e.$$legacy;return function(K,S){return arguments.length>0?((!S||T||k)&&f(S?I():K),K):I()}}var j=!1,C=(r&ii?on:Ko)(()=>(j=!1,I()));o&&a(C);var O=Je;return function(K,S){if(arguments.length>0){const m=S?a(C):o?mt(K):K;return v(C,m),j=!0,l!==void 0&&(l=m),K}return Rr&&j||O.f&gr?C.v:a(C)}}function Ml(e){Wt===null&&Po(),Lt(()=>{const t=Sa(e);if(typeof t=="function")return t})}function Nl(e){Wt===null&&Po(),Ml(()=>()=>Sa(e))}const Tl="5";var Co;typeof window<"u"&&((Co=window.__svelte??(window.__svelte={})).v??(Co.v=new Set)).add(Tl);const ao="prx-console-token",Pl=[{labelKey:"nav.overview",path:"/overview"},{labelKey:"nav.sessions",path:"/sessions"},{labelKey:"nav.channels",path:"/channels"},{labelKey:"nav.hooks",path:"/hooks"},{labelKey:"nav.mcp",path:"/mcp"},{labelKey:"nav.skills",path:"/skills"},{labelKey:"nav.plugins",path:"/plugins"},{labelKey:"nav.config",path:"/config"},{labelKey:"nav.logs",path:"/logs"}];function Ra(){var e;return typeof window>"u"?"":((e=window.localStorage.getItem(ao))==null?void 0:e.trim())??""}function Ol(e){typeof window>"u"||window.localStorage.setItem(ao,e.trim())}function As(){typeof window>"u"||window.localStorage.removeItem(ao)}function $s(){return typeof window>"u"?"/":window.location.pathname||"/"}function $r(e,t=!1){if(typeof window>"u")return;e.startsWith("/")||(e=`/${e}`);const r=t?"replaceState":"pushState";window.location.pathname!==e&&(window.history[r]({},"",e),window.dispatchEvent(new PopStateEvent("popstate")))}function Fl(e){if(typeof window>"u")return()=>{};const t=()=>{e($s())};return window.addEventListener("popstate",t),t(),()=>{window.removeEventListener("popstate",t)}}const _n="".trim(),Xa=_n.endsWith("/")?_n.slice(0,-1):_n;class ko extends Error{constructor(t,r){super(r),this.name="ApiError",this.status=t}}async function Il(e){return(e.headers.get("content-type")||"").includes("application/json")?e.json().catch(()=>null):e.text().catch(()=>null)}function Ll(e,t){return e&&typeof e=="object"&&typeof e.error=="string"?e.error:`Request failed (${t})`}async function kt(e,t={}){const r=Ra(),n={Accept:"application/json",...t.headers};r&&(n.Authorization=`Bearer ${r}`),t.body&&!(t.body instanceof FormData)&&!n["Content-Type"]&&(n["Content-Type"]="application/json");const o=await fetch(`${Xa}${e}`,{...t,headers:n}),s=await Il(o);if(o.status===401)throw As(),$r("/",!0),new ko(401,"Unauthorized");if(!o.ok)throw new ko(o.status,Ll(s,o.status));return s}const ht={getStatus:()=>kt("/api/status"),getSessions:()=>kt("/api/sessions"),getSessionMessages:e=>kt(`/api/sessions/${encodeURIComponent(e)}/messages`),sendMessage:(e,t)=>kt(`/api/sessions/${encodeURIComponent(e)}/message`,{method:"POST",body:JSON.stringify({message:t})}),sendMessageWithMedia:(e,t,r=[])=>{if(!Array.isArray(r)||r.length===0)return ht.sendMessage(e,t);const n=new FormData;n.append("message",t);for(const o of r)n.append("files",o);return kt(`/api/sessions/${encodeURIComponent(e)}/message`,{method:"POST",body:n})},getSessionMediaUrl:e=>{const t=new URLSearchParams({path:e}),r=Ra();return r&&t.set("token",r),`${Xa}/api/sessions/media?${t.toString()}`},getChannelsStatus:()=>kt("/api/channels/status"),getConfig:()=>kt("/api/config"),saveConfig:e=>kt("/api/config",{method:"POST",body:JSON.stringify(e)}),getHooks:()=>kt("/api/hooks"),createHook:e=>kt("/api/hooks",{method:"POST",body:JSON.stringify(e)}),updateHook:(e,t)=>kt(`/api/hooks/${encodeURIComponent(e)}`,{method:"PUT",body:JSON.stringify(t)}),deleteHook:e=>kt(`/api/hooks/${encodeURIComponent(e)}`,{method:"DELETE"}),toggleHook:e=>kt(`/api/hooks/${encodeURIComponent(e)}/toggle`,{method:"PATCH"}),getMcpServers:()=>kt("/api/mcp/servers"),getSkills:()=>kt("/api/skills"),discoverSkills:(e="github",t="")=>{const r=new URLSearchParams;return e&&r.set("source",e),t&&r.set("query",t),kt(`/api/skills/discover?${r.toString()}`)},installSkill:(e,t)=>kt("/api/skills/install",{method:"POST",body:JSON.stringify({url:e,name:t})}),uninstallSkill:e=>kt(`/api/skills/${encodeURIComponent(e)}`,{method:"DELETE"}),toggleSkill:e=>kt(`/api/skills/${encodeURIComponent(e)}/toggle`,{method:"PATCH"}),getPlugins:()=>kt("/api/plugins"),reloadPlugin:e=>kt(`/api/plugins/${encodeURIComponent(e)}/reload`,{method:"POST"})},Qa={provider:{label:"Provider 设置",defaultOpen:!0,fields:{api_key:{type:"string",sensitive:!0,label:"API Key",desc:"当前 Provider 的 API 密钥。修改后需要重启生效",default:""},api_url:{type:"string",label:"API URL",desc:"自定义 API 端点地址。留空使用 Provider 默认值（如 Ollama 填 http://localhost:11434）",default:""},default_provider:{type:"enum",label:"默认 Provider",desc:"选择 AI 模型提供商。决定使用哪个 API 来处理请求",default:"openrouter",options:["openrouter","anthropic","openai","ollama","gemini","groq","glm","xai","compatible","copilot","claude-cli","dashscope","dashscope-coding-intl","deepseek","fireworks","mistral","together"]},default_model:{type:"string",label:"默认模型",desc:"默认使用的模型名称（如 anthropic/claude-sonnet-4-6）",default:"anthropic/claude-sonnet-4.6"},default_temperature:{type:"number",label:"温度",desc:"模型输出的随机性（0=确定性，2=最随机）。推荐日常对话 0.7，代码任务 0.3",default:.7,min:0,max:2,step:.1}}},gateway:{label:"Gateway 网关",defaultOpen:!0,fields:{"gateway.port":{type:"number",label:"端口",desc:"Gateway HTTP 服务端口号",default:3e3,min:1,max:65535},"gateway.host":{type:"string",label:"监听地址",desc:"绑定的 IP 地址。127.0.0.1 仅本机访问，0.0.0.0 允许外部访问",default:"127.0.0.1"},"gateway.require_pairing":{type:"bool",label:"需要配对",desc:"开启后必须先配对才能访问 API。关闭则任何人可直接访问（不安全）",default:!0},"gateway.allow_public_bind":{type:"bool",label:"允许公网绑定",desc:"允许绑定到非 localhost 地址而不需要隧道。通常不建议开启",default:!1},"gateway.trust_forwarded_headers":{type:"bool",label:"信任代理头",desc:"信任 X-Forwarded-For / X-Real-IP 头。仅在反向代理后方启用",default:!1},"gateway.request_timeout_secs":{type:"number",label:"请求超时(秒)",desc:"HTTP 请求处理超时时间",default:60,min:5,max:600},"gateway.pair_rate_limit_per_minute":{type:"number",label:"配对速率限制(/分)",desc:"每客户端每分钟最大配对请求数",default:10,min:1,max:100},"gateway.webhook_rate_limit_per_minute":{type:"number",label:"Webhook 速率限制(/分)",desc:"每客户端每分钟最大 Webhook 请求数",default:60,min:1,max:1e3}}},channels:{label:"消息通道",defaultOpen:!0,fields:{"channels_config.message_timeout_secs":{type:"number",label:"消息处理超时(秒)",desc:"单条消息处理的最大超时时间（LLM + 工具调用）",default:300,min:30,max:3600},"channels_config.cli":{type:"bool",label:"CLI 交互模式",desc:"启用命令行交互通道",default:!0}}},agent:{label:"Agent 编排",defaultOpen:!1,fields:{"agent.max_tool_iterations":{type:"number",label:"最大工具循环次数",desc:"每条用户消息最多执行多少轮工具调用。设 0 回退到默认 10",default:10,min:0,max:100},"agent.max_history_messages":{type:"number",label:"最大历史消息数",desc:"每个会话保留的历史消息条数",default:50,min:5,max:500},"agent.parallel_tools":{type:"bool",label:"并行工具执行",desc:"允许在单次迭代中并行调用多个工具",default:!1},"agent.compact_context":{type:"bool",label:"紧凑上下文",desc:"为小模型（13B 以下）减少上下文大小",default:!1},"agent.compaction.mode":{type:"enum",label:"上下文压缩模式",desc:"off=不压缩，safeguard=保守压缩（默认），aggressive=激进截断",default:"safeguard",options:["off","safeguard","aggressive"]},"agent.compaction.max_context_tokens":{type:"number",label:"最大上下文 Token",desc:"触发压缩的 Token 阈值",default:128e3,min:1e3,max:1e6},"agent.compaction.keep_recent_messages":{type:"number",label:"压缩后保留消息数",desc:"压缩后保留最近的非系统消息数量",default:12,min:1,max:100},"agent.compaction.memory_flush":{type:"bool",label:"压缩前刷新记忆",desc:"在压缩之前提取并保存记忆",default:!0}}},memory:{label:"记忆存储",defaultOpen:!1,fields:{"memory.backend":{type:"enum",label:"存储后端",desc:"记忆存储引擎类型",default:"sqlite",options:["sqlite","postgres","markdown","lucid","none"]},"memory.auto_save":{type:"bool",label:"自动保存",desc:"自动保存用户输入到记忆",default:!0},"memory.hygiene_enabled":{type:"bool",label:"记忆清理",desc:"定期运行记忆归档和保留清理",default:!0},"memory.archive_after_days":{type:"number",label:"归档天数",desc:"超过此天数的日志/会话文件将被归档",default:7,min:1,max:365},"memory.purge_after_days":{type:"number",label:"清除天数",desc:"归档文件超过此天数后被清除",default:30,min:1,max:3650},"memory.conversation_retention_days":{type:"number",label:"对话保留天数",desc:"SQLite 后端：超过此天数的对话记录被清理",default:3,min:1,max:365},"memory.embedding_provider":{type:"enum",label:"嵌入提供商",desc:"记忆向量化的嵌入模型提供商",default:"none",options:["none","openai","custom"]},"memory.embedding_model":{type:"string",label:"嵌入模型",desc:"嵌入模型名称（如 text-embedding-3-small）",default:"text-embedding-3-small"},"memory.embedding_dimensions":{type:"number",label:"嵌入维度",desc:"嵌入向量的维度数",default:1536,min:64,max:4096},"memory.vector_weight":{type:"number",label:"向量权重",desc:"混合搜索中向量相似度的权重（0-1）",default:.7,min:0,max:1,step:.1},"memory.keyword_weight":{type:"number",label:"关键词权重",desc:"混合搜索中 BM25 关键词匹配的权重（0-1）",default:.3,min:0,max:1,step:.1},"memory.min_relevance_score":{type:"number",label:"最低相关性分数",desc:"低于此分数的记忆不会注入上下文",default:.4,min:0,max:1,step:.05},"memory.snapshot_enabled":{type:"bool",label:"记忆快照",desc:"定期将核心记忆导出为 MEMORY_SNAPSHOT.md",default:!1},"memory.auto_hydrate":{type:"bool",label:"自动恢复",desc:"当 brain.db 不存在时自动从快照恢复",default:!0}}},security:{label:"安全策略",defaultOpen:!1,fields:{"autonomy.level":{type:"enum",label:"自主级别",desc:"read_only=只读，supervised=需审批（默认），full=完全自主",default:"supervised",options:["read_only","supervised","full"]},"autonomy.workspace_only":{type:"bool",label:"仅工作区",desc:"限制文件写入和命令执行在工作区目录内",default:!0},"autonomy.max_actions_per_hour":{type:"number",label:"每小时最大操作数",desc:"每小时允许的最大操作次数",default:20,min:1,max:1e4},"autonomy.require_approval_for_medium_risk":{type:"bool",label:"中风险需审批",desc:"中等风险的 Shell 命令需要明确批准",default:!0},"autonomy.block_high_risk_commands":{type:"bool",label:"阻止高风险命令",desc:"即使在白名单中也阻止高风险命令",default:!0},"autonomy.allowed_commands":{type:"array",label:"允许的命令",desc:"允许执行的命令白名单",default:["git","npm","cargo","ls","cat","grep","find","echo"]},"secrets.encrypt":{type:"bool",label:"加密密钥",desc:"对 config.toml 中的 API Key 和 Token 进行加密存储",default:!0}}},heartbeat:{label:"心跳检测",defaultOpen:!1,fields:{"heartbeat.enabled":{type:"bool",label:"启用心跳",desc:"启用定期心跳检查",default:!1},"heartbeat.interval_minutes":{type:"number",label:"间隔(分钟)",desc:"心跳检查的时间间隔",default:30,min:1,max:1440},"heartbeat.active_hours":{type:"array",label:"活跃时段",desc:"心跳检查的有效小时范围（如 [8, 23]）",default:[8,23]},"heartbeat.prompt":{type:"string",label:"心跳提示词",desc:"心跳触发时使用的提示词",default:"Check HEARTBEAT.md and follow instructions."}}},reliability:{label:"可靠性",defaultOpen:!1,fields:{"reliability.provider_retries":{type:"number",label:"Provider 重试次数",desc:"调用 Provider 失败后的重试次数",default:2,min:0,max:10},"reliability.provider_backoff_ms":{type:"number",label:"重试退避(ms)",desc:"Provider 重试的基础退避时间",default:500,min:100,max:3e4},"reliability.fallback_providers":{type:"array",label:"备用 Provider",desc:"主 Provider 不可用时按顺序尝试的备用列表",default:[]},"reliability.api_keys":{type:"array",label:"轮换 API Key",desc:"遇到速率限制时轮换使用的额外 API Key",default:[]},"reliability.channel_initial_backoff_secs":{type:"number",label:"通道初始退避(秒)",desc:"通道/守护进程重启的初始退避时间",default:2,min:1,max:60},"reliability.channel_max_backoff_secs":{type:"number",label:"通道最大退避(秒)",desc:"通道/守护进程重启的最大退避时间",default:60,min:5,max:3600}}},scheduler:{label:"调度器",defaultOpen:!1,fields:{"scheduler.enabled":{type:"bool",label:"启用调度器",desc:"启用内置定时任务调度循环",default:!0},"scheduler.max_tasks":{type:"number",label:"最大任务数",desc:"最多持久化保存的计划任务数量",default:64,min:1,max:1e3},"scheduler.max_concurrent":{type:"number",label:"最大并发数",desc:"每次调度周期内最多执行的任务数",default:4,min:1,max:32},"cron.enabled":{type:"bool",label:"启用 Cron",desc:"启用 Cron 子系统",default:!0},"cron.max_run_history":{type:"number",label:"Cron 历史记录数",desc:"保留的 Cron 运行历史记录条数",default:50,min:10,max:1e3}}},sessions_spawn:{label:"子进程管理",defaultOpen:!1,fields:{"sessions_spawn.default_mode":{type:"enum",label:"默认模式",desc:"子进程默认执行模式",default:"task",options:["task","process"]},"sessions_spawn.max_concurrent":{type:"number",label:"最大并发数",desc:"全局最大并发子进程/任务数",default:4,min:1,max:32},"sessions_spawn.max_spawn_depth":{type:"number",label:"最大嵌套深度",desc:"子进程可以再次 spawn 的最大深度",default:2,min:1,max:10},"sessions_spawn.max_children_per_agent":{type:"number",label:"每父进程最大子数",desc:"每个父会话允许的最大并发子运行数",default:5,min:1,max:20},"sessions_spawn.cleanup_on_complete":{type:"bool",label:"完成后清理",desc:"进程模式完成后删除工作区目录",default:!0}}},observability:{label:"可观测性",defaultOpen:!1,fields:{"observability.backend":{type:"enum",label:"后端",desc:"可观测性后端类型",default:"none",options:["none","log","prometheus","otel"]},"observability.otel_endpoint":{type:"string",label:"OTLP 端点",desc:"OpenTelemetry Collector 端点 URL（仅 otel 后端）",default:""},"observability.otel_service_name":{type:"string",label:"服务名称",desc:"上报给 OTel 的服务名称",default:"openprx"}}},web_search:{label:"网络搜索",defaultOpen:!1,fields:{"web_search.enabled":{type:"bool",label:"启用搜索",desc:"启用网络搜索工具",default:!1},"web_search.provider":{type:"enum",label:"搜索引擎",desc:"搜索提供商。DuckDuckGo 免费无 Key，Brave 需要 API Key",default:"duckduckgo",options:["duckduckgo","brave"]},"web_search.brave_api_key":{type:"string",sensitive:!0,label:"Brave API Key",desc:"Brave Search API 密钥（选 Brave 时必填）",default:""},"web_search.max_results":{type:"number",label:"最大结果数",desc:"每次搜索返回的最大结果数（1-10）",default:5,min:1,max:10},"web_search.fetch_enabled":{type:"bool",label:"启用页面抓取",desc:"允许抓取和提取网页可读内容",default:!0},"web_search.fetch_max_chars":{type:"number",label:"抓取最大字符",desc:"网页抓取返回的最大字符数",default:1e4,min:100,max:1e5}}},cost:{label:"成本控制",defaultOpen:!1,fields:{"cost.enabled":{type:"bool",label:"启用成本追踪",desc:"启用 API 调用成本追踪和预算控制",default:!1},"cost.daily_limit_usd":{type:"number",label:"日限额(USD)",desc:"每日消费上限（美元）",default:10,min:.1,max:1e4,step:.1},"cost.monthly_limit_usd":{type:"number",label:"月限额(USD)",desc:"每月消费上限（美元）",default:100,min:1,max:1e5,step:1},"cost.warn_at_percent":{type:"number",label:"预警百分比",desc:"消费达到限额的多少百分比时发出警告",default:80,min:10,max:100}}},runtime:{label:"运行时",defaultOpen:!1,fields:{"runtime.kind":{type:"enum",label:"运行时类型",desc:"命令执行环境：native=本机，docker=容器隔离",default:"native",options:["native","docker"]},"runtime.reasoning_enabled":{type:"enum",label:"推理模式",desc:"全局推理/思考模式：null=Provider 默认，true=启用，false=禁用",default:"",options:["","true","false"]}}},tunnel:{label:"隧道",defaultOpen:!1,fields:{"tunnel.provider":{type:"enum",label:"隧道类型",desc:"将 Gateway 暴露到公网的隧道服务",default:"none",options:["none","cloudflare","tailscale","ngrok","custom"]}}},identity:{label:"身份格式",defaultOpen:!1,fields:{"identity.format":{type:"enum",label:"身份格式",desc:"OpenClaw 或 AIEOS 身份文档格式",default:"openclaw",options:["openclaw","aieos"]}}}};function Hn(e){return String(e).replace(/_/g," ").replace(/\b\w/g,t=>t.toUpperCase())}function Rl(){const e=new Set;for(const t of Object.values(Qa))for(const r of Object.keys(t.fields))e.add(r.split(".")[0]);return e}const Cs=Rl();function Dn(e){const t=Object.entries(Qa).map(([n,o])=>({groupKey:n,label:o.label,dynamic:!1}));if(!e||typeof e!="object")return t;const r=Object.keys(e).filter(n=>!Cs.has(n)).sort().map(n=>({groupKey:n,label:Hn(n),dynamic:!0}));return[...t,...r]}function Za(e){return`config-section-${e}`}function Ms(e){if(typeof document>"u"||typeof window>"u")return;const t=document.getElementById(Za(e));t instanceof HTMLDetailsElement&&(t.open=!0),t&&t.scrollIntoView({behavior:"smooth",block:"start"});const r=`#${Za(e)}`;window.location.hash!==r&&(window.location.hash=r)}const jl={title:"PRX Console",menu:"Menu",closeSidebar:"Close sidebar",language:"Language",notFound:"Not found",backToOverview:"Back to Overview"},Hl={overview:"Overview",sessions:"Sessions",channels:"Channels",config:"Config",hooks:"Hooks",mcp:"MCP",skills:"Skills",plugins:"Plugins",logs:"Logs"},Dl={logout:"Logout",loading:"Loading...",error:"Error",refresh:"Refresh",updatedAt:"Updated {time}",na:"N/A",enabled:"Enabled",disabled:"Disabled",yes:"Yes",no:"No",unknown:"Unknown",clipboardUnavailable:"Clipboard not available.",copied:"Copied",copyFailed:"Copy failed",empty:"Empty"},Ul={title:"Overview",version:"Version",uptime:"Uptime",model:"Model",memoryBackend:"Memory Backend",gatewayPort:"Gateway Port",configuredChannels:"Configured Channels",loading:"Loading status...",loadFailed:"Failed to load status.",noChannelsConfigured:"No channels configured."},zl={title:"Sessions",sessionId:"Session ID",sender:"Sender",channel:"Channel",messages:"Messages",lastMessage:"Last Message",loading:"Loading sessions...",loadFailed:"Failed to load sessions.",none:"No active sessions"},Bl={title:"Chat",session:"Session",back:"Back to Sessions",loading:"Loading messages...",loadFailed:"Failed to load messages.",sendFailed:"Failed to send message.",empty:"No messages in this session.",inputPlaceholder:"Type a message...",send:"Send",sending:"Sending..."},Wl={title:"Channels",type:"Type",status:"Status",loading:"Loading channels...",loadFailed:"Failed to load channel status.",noChannels:"No channels available.",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"Email",irc:"IRC",lark:"Lark",dingtalk:"DingTalk",qq:"QQ",cli:"CLI",configured:"Configured"}},ql={title:"Config",rawJson:"Raw JSON",structured:"Structured View",copy:"Copy",copyJson:"Copy JSON",loading:"Loading config...",loadFailed:"Failed to load config.",section:{general:"General",gateway:"Gateway",channels:"Channels",memory:"Memory",security:"Security",model:"Model",other:"Other"},field:{version:"Version",runtimeModel:"Runtime Model",memoryBackend:"Memory Backend",configuredChannels:"Configured Channels",notConfigured:"Not configured",notSet:"Not set"},channel:{settings:"settings",notConfigured:"Not configured"},redacted:"Redacted",emptyObject:"No settings"},Vl={title:"Logs",connected:"Connected",disconnected:"Disconnected",reconnecting:"Reconnecting",pause:"Pause",resume:"Resume",clear:"Clear",waiting:"Waiting for log stream..."},Gl={title:"Hooks",loading:"Loading hooks...",loadFailed:"Failed to load hooks.",noHooks:"No hooks configured.",addHook:"Add Hook",cancelAdd:"Cancel",newHook:"New Hook",event:"Event",command:"Command",commandPlaceholder:"e.g. /opt/scripts/on-event.sh",timeout:"Timeout (ms)",enabled:"Enabled",globalToggleHint:"Enabled state is currently controlled globally by the backend.",edit:"Edit",delete:"Delete",deleting:"Deleting...",save:"Save",saving:"Saving...",cancel:"Cancel",commandRequired:"Command is required.",timeoutInvalid:"Timeout must be at least 1000 ms.",saveFailed:"Failed to save hook.",deleteFailed:"Failed to delete hook.",toggleFailed:"Failed to update hook state."},Kl={title:"MCP Servers",loading:"Loading MCP servers...",loadFailed:"Failed to load MCP servers.",noServers:"No MCP servers configured.",connected:"Connected",connecting:"Connecting",disconnected:"Disconnected",tools:"tools",availableTools:"Available Tools",noTools:"No tools available."},Jl={title:"Skills",loading:"Loading skills...",noSkills:"No skills installed.",active:"active",tabInstalled:"Installed",tabDiscover:"Discover",search:"Search skills...",source:"Source",searchBtn:"Search",searching:"Searching...",loadFailed:"Failed to load skills.",searchFailed:"Failed to search skills.",noResults:"No results found.",install:"Install",installing:"Installing...",installed:"Installed",uninstall:"Uninstall",uninstalling:"Removing...",confirmUninstall:'Are you sure you want to uninstall "{name}"?',stars:"stars",owner:"by",licensed:"Licensed",unlicensed:"No license",readOnlyState:"Enable state is read-only.",installSuccess:"Skill installed successfully",installFailed:"Failed to install skill",uninstallSuccess:"Skill uninstalled",uninstallFailed:"Failed to uninstall skill"},Yl={title:"Plugins",loading:"Loading plugins...",loadFailed:"Failed to load plugins.",noPlugins:"No WASM plugins loaded.",capabilities:"Capabilities",permissions:"Permissions",statusActive:"Active",reload:"Reload",reloadSuccess:'Plugin "{name}" reloaded',reloadFailed:"Failed to reload plugin"},Xl={title:"PRX Console Login",accessToken:"Access Token",login:"Login",hint:"Enter your gateway auth token to continue.",placeholder:"Bearer token",tokenRequired:"Access token is required."},Ql={app:jl,nav:Hl,common:Dl,overview:Ul,sessions:zl,chat:Bl,channels:Wl,config:ql,logs:Vl,hooks:Gl,mcp:Kl,skills:Jl,plugins:Yl,login:Xl},Zl={title:"PRX 控制台",menu:"菜单",closeSidebar:"关闭侧边栏",language:"语言",notFound:"页面未找到",backToOverview:"返回概览"},ed={overview:"概览",sessions:"会话",channels:"通道",config:"配置",hooks:"Hooks",mcp:"MCP",skills:"Skills",plugins:"插件",logs:"日志"},td={logout:"退出登录",loading:"加载中...",error:"错误",refresh:"刷新",updatedAt:"更新时间 {time}",na:"暂无",enabled:"已启用",disabled:"已禁用",yes:"是",no:"否",unknown:"未知",clipboardUnavailable:"当前环境不支持剪贴板。",copied:"已复制",copyFailed:"复制失败",empty:"空"},rd={title:"概览",version:"版本",uptime:"运行时长",model:"模型",memoryBackend:"记忆后端",gatewayPort:"网关端口",configuredChannels:"已配置通道",loading:"正在加载状态...",loadFailed:"加载状态失败。",noChannelsConfigured:"尚未配置任何通道。"},ad={title:"会话",sessionId:"会话 ID",sender:"发送方",channel:"通道",messages:"消息数",lastMessage:"最后消息",loading:"正在加载会话...",loadFailed:"加载会话失败。",none:"当前没有活跃会话"},nd={title:"聊天",session:"会话",back:"返回会话列表",loading:"正在加载消息...",loadFailed:"加载消息失败。",sendFailed:"发送消息失败。",empty:"此会话暂无消息。",inputPlaceholder:"输入消息...",send:"发送",sending:"发送中..."},od={title:"通道",type:"类型",status:"状态",loading:"正在加载通道状态...",loadFailed:"加载通道状态失败。",noChannels:"暂无通道数据。",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"邮件",irc:"IRC",lark:"飞书",dingtalk:"钉钉",qq:"QQ",cli:"命令行",configured:"已配置"}},sd={title:"配置",rawJson:"原始 JSON",structured:"结构化视图",copy:"复制",copyJson:"复制 JSON",loading:"正在加载配置...",loadFailed:"加载配置失败。",section:{general:"常规",gateway:"网关",channels:"通道",memory:"记忆",security:"安全",model:"模型",other:"其他"},field:{version:"版本",runtimeModel:"运行模型",memoryBackend:"记忆后端",configuredChannels:"已配置通道",notConfigured:"未配置",notSet:"未设置"},channel:{settings:"配置",notConfigured:"未配置"},redacted:"已脱敏",emptyObject:"无配置项"},id={title:"日志",connected:"已连接",disconnected:"已断开",reconnecting:"重连中",pause:"暂停",resume:"继续",clear:"清空",waiting:"等待日志流..."},ld={title:"Hooks",loading:"正在加载 Hooks...",loadFailed:"加载 Hooks 失败。",noHooks:"尚未配置任何 Hook。",addHook:"添加 Hook",cancelAdd:"取消",newHook:"新建 Hook",event:"事件",command:"命令",commandPlaceholder:"例如 /opt/scripts/on-event.sh",timeout:"超时 (ms)",enabled:"启用",globalToggleHint:"当前启用状态由后端全局控制。",edit:"编辑",delete:"删除",deleting:"删除中...",save:"保存",saving:"保存中...",cancel:"取消",commandRequired:"命令不能为空。",timeoutInvalid:"超时必须至少为 1000 毫秒。",saveFailed:"保存 Hook 失败。",deleteFailed:"删除 Hook 失败。",toggleFailed:"更新 Hook 状态失败。"},dd={title:"MCP 服务",loading:"正在加载 MCP 服务...",loadFailed:"加载 MCP 服务失败。",noServers:"尚未配置任何 MCP 服务。",connected:"已连接",connecting:"连接中",disconnected:"已断开",tools:"个工具",availableTools:"可用工具",noTools:"无可用工具。"},cd={title:"Skills",loading:"正在加载 Skills...",noSkills:"尚未安装任何 Skill。",active:"已启用",tabInstalled:"已安装",tabDiscover:"发现新 Skills",search:"搜索 Skills...",source:"来源",searchBtn:"搜索",searching:"搜索中...",loadFailed:"加载 Skills 失败。",searchFailed:"搜索 Skill 失败。",noResults:"未找到结果。",install:"安装",installing:"安装中...",installed:"已安装",uninstall:"卸载",uninstalling:"卸载中...",confirmUninstall:'确定要卸载 "{name}" 吗？',stars:"星标",owner:"作者",licensed:"有许可证",unlicensed:"无许可证",readOnlyState:"启用状态当前为只读展示。",installSuccess:"Skill 安装成功",installFailed:"Skill 安装失败",uninstallSuccess:"Skill 已卸载",uninstallFailed:"Skill 卸载失败"},ud={title:"插件",loading:"正在加载插件...",loadFailed:"加载插件失败。",noPlugins:"未加载任何 WASM 插件。",capabilities:"能力",permissions:"权限",statusActive:"运行中",reload:"重载",reloadSuccess:'插件 "{name}" 已重载',reloadFailed:"插件重载失败"},fd={title:"PRX 控制台登录",accessToken:"访问令牌",login:"登录",hint:"请输入网关认证令牌以继续。",placeholder:"Bearer 令牌",tokenRequired:"访问令牌不能为空。"},vd={app:Zl,nav:ed,common:td,overview:rd,sessions:ad,chat:nd,channels:od,config:sd,logs:id,hooks:ld,mcp:dd,skills:cd,plugins:ud,login:fd},un="prx-console-lang",ja="en",xn={en:Ql,zh:vd};function Un(e){return typeof e!="string"||e.trim().length===0?ja:e.trim().toLowerCase().startsWith("zh")?"zh":"en"}function gd(){var e;if(typeof window<"u"){const t=window.localStorage.getItem(un);if(t)return Un(t)}if(typeof navigator<"u"){const t=navigator.language||((e=navigator.languages)==null?void 0:e[0])||ja;return Un(t)}return ja}function wo(e,t){return t.split(".").reduce((r,n)=>{if(!(!r||typeof r!="object"))return r[n]},e)}function Ns(e){typeof document<"u"&&(document.documentElement.lang=e==="zh"?"zh-CN":"en")}function pd(e){typeof window<"u"&&window.localStorage.setItem(un,e)}const Qr=mt({lang:gd()});Ns(Qr.lang);function Ts(e){const t=Un(e);Qr.lang!==t&&(Qr.lang=t,pd(t),Ns(t))}function na(){Ts(Qr.lang==="en"?"zh":"en")}function yd(){if(typeof window>"u")return;const e=window.localStorage.getItem(un);e&&Ts(e)}function h(e,t={}){const r=xn[Qr.lang]??xn[ja];let n=wo(r,e);if(typeof n!="string"&&(n=wo(xn[ja],e)),typeof n!="string")return e;for(const[o,s]of Object.entries(t))n=n.replaceAll(`{${o}}`,String(s));return n}/**
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
 */const bd={xmlns:"http://www.w3.org/2000/svg",width:24,height:24,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor","stroke-width":2,"stroke-linecap":"round","stroke-linejoin":"round"};/**
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
 */const hd=e=>{for(const t in e)if(t.startsWith("aria-")||t==="role"||t==="title")return!0;return!1};var md=sl("<svg><!><!></svg>");function pt(e,t){Te(t,!0);const r=aa(t,"color",3,"currentColor"),n=aa(t,"size",3,24),o=aa(t,"strokeWidth",3,2),s=aa(t,"absoluteStrokeWidth",3,!1),l=aa(t,"iconNode",19,()=>[]),d=ut(t,["$$slots","$$events","$$legacy","name","color","size","strokeWidth","absoluteStrokeWidth","iconNode","children"]);var c=md();mo(c,(w,k)=>({...bd,...w,...d,width:n(),height:n(),stroke:r(),"stroke-width":k,class:["lucide-icon lucide",t.name&&`lucide-${t.name}`,t.class]}),[()=>!t.children&&!hd(d)&&{"aria-hidden":"true"},()=>s()?Number(o())*24/Number(n()):o()]);var f=i(c);Xe(f,17,l,rt,(w,k)=>{var I=ce(()=>Na(a(k),2));let T=()=>a(I)[0],j=()=>a(I)[1];var C=Re(),O=xe(C);pl(O,T,!0,(q,K)=>{mo(q,()=>({...j()}))}),g(w,C)});var _=p(f);ct(_,()=>t.children??Ce),g(e,c),Pe()}function _d(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3.85 8.62a4 4 0 0 1 4.78-4.77 4 4 0 0 1 6.74 0 4 4 0 0 1 4.78 4.78 4 4 0 0 1 0 6.74 4 4 0 0 1-4.77 4.78 4 4 0 0 1-6.75 0 4 4 0 0 1-4.78-4.77 4 4 0 0 1 0-6.76Z"}],["path",{d:"m9 12 2 2 4-4"}]];pt(e,gt({name:"badge-check"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function So(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M10 22V7a1 1 0 0 0-1-1H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-5a1 1 0 0 0-1-1H2"}],["rect",{x:"14",y:"2",width:"8",height:"8",rx:"1"}]];pt(e,gt({name:"blocks"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function xd(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 8V4H8"}],["rect",{width:"16",height:"12",x:"4",y:"8",rx:"2"}],["path",{d:"M2 14h2"}],["path",{d:"M20 14h2"}],["path",{d:"M15 13v2"}],["path",{d:"M9 13v2"}]];pt(e,gt({name:"bot"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function kd(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 18V5"}],["path",{d:"M15 13a4.17 4.17 0 0 1-3-4 4.17 4.17 0 0 1-3 4"}],["path",{d:"M17.598 6.5A3 3 0 1 0 12 5a3 3 0 1 0-5.598 1.5"}],["path",{d:"M17.997 5.125a4 4 0 0 1 2.526 5.77"}],["path",{d:"M18 18a4 4 0 0 0 2-7.464"}],["path",{d:"M19.967 17.483A4 4 0 1 1 12 18a4 4 0 1 1-7.967-.517"}],["path",{d:"M6 18a4 4 0 0 1-2-7.464"}],["path",{d:"M6.003 5.125a4 4 0 0 0-2.526 5.77"}]];pt(e,gt({name:"brain"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function wd(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M17 19a1 1 0 0 1-1-1v-2a2 2 0 0 1 2-2h2a2 2 0 0 1 2 2v2a1 1 0 0 1-1 1z"}],["path",{d:"M17 21v-2"}],["path",{d:"M19 14V6.5a1 1 0 0 0-7 0v11a1 1 0 0 1-7 0V10"}],["path",{d:"M21 21v-2"}],["path",{d:"M3 5V3"}],["path",{d:"M4 10a2 2 0 0 1-2-2V6a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2a2 2 0 0 1-2 2z"}],["path",{d:"M7 5V3"}]];pt(e,gt({name:"cable"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Sd(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3 3v16a2 2 0 0 0 2 2h16"}],["path",{d:"M18 17V9"}],["path",{d:"M13 17V5"}],["path",{d:"M8 17v-3"}]];pt(e,gt({name:"chart-column"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Ed(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["line",{x1:"12",x2:"12",y1:"8",y2:"12"}],["line",{x1:"12",x2:"12.01",y1:"16",y2:"16"}]];pt(e,gt({name:"circle-alert"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Ad(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M21.801 10A10 10 0 1 1 17 3.335"}],["path",{d:"m9 11 3 3L22 4"}]];pt(e,gt({name:"circle-check-big"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function $d(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["path",{d:"M12 6v6l4 2"}]];pt(e,gt({name:"clock"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Cd(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m18 16 4-4-4-4"}],["path",{d:"m6 8-4 4 4 4"}],["path",{d:"m14.5 4-5 16"}]];pt(e,gt({name:"code-xml"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Md(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["ellipse",{cx:"12",cy:"5",rx:"9",ry:"3"}],["path",{d:"M3 5V19A9 3 0 0 0 21 19V5"}],["path",{d:"M3 12A9 3 0 0 0 21 12"}]];pt(e,gt({name:"database"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Nd(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["line",{x1:"12",x2:"12",y1:"2",y2:"22"}],["path",{d:"M17 5H9.5a3.5 3.5 0 0 0 0 7h5a3.5 3.5 0 0 1 0 7H6"}]];pt(e,gt({name:"dollar-sign"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Td(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M15 6a9 9 0 0 0-9 9V3"}],["circle",{cx:"18",cy:"6",r:"3"}],["circle",{cx:"6",cy:"18",r:"3"}]];pt(e,gt({name:"git-branch"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Pd(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["path",{d:"M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"}],["path",{d:"M2 12h20"}]];pt(e,gt({name:"globe"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Od(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M2 9.5a5.5 5.5 0 0 1 9.591-3.676.56.56 0 0 0 .818 0A5.49 5.49 0 0 1 22 9.5c0 2.29-1.5 4-3 5.5l-5.492 5.313a2 2 0 0 1-3 .019L5 15c-1.5-1.5-3-3.2-3-5.5"}],["path",{d:"M3.22 13H9.5l.5-1 2 4.5 2-7 1.5 3.5h5.27"}]];pt(e,gt({name:"heart-pulse"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Fd(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 2v4"}],["path",{d:"m16.2 7.8 2.9-2.9"}],["path",{d:"M18 12h4"}],["path",{d:"m16.2 16.2 2.9 2.9"}],["path",{d:"M12 18v4"}],["path",{d:"m4.9 19.1 2.9-2.9"}],["path",{d:"M2 12h4"}],["path",{d:"m4.9 4.9 2.9 2.9"}]];pt(e,gt({name:"loader"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Id(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z"}]];pt(e,gt({name:"message-square"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Ld(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401"}]];pt(e,gt({name:"moon"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Rd(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m16 6-8.414 8.586a2 2 0 0 0 2.829 2.829l8.414-8.586a4 4 0 1 0-5.657-5.657l-8.379 8.551a6 6 0 1 0 8.485 8.485l8.379-8.551"}]];pt(e,gt({name:"paperclip"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Ps(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"}],["path",{d:"M21 3v5h-5"}],["path",{d:"M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"}],["path",{d:"M8 16H3v5"}]];pt(e,gt({name:"refresh-cw"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function jd(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m21 21-4.34-4.34"}],["circle",{cx:"11",cy:"11",r:"8"}]];pt(e,gt({name:"search"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Hd(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915"}],["circle",{cx:"12",cy:"12",r:"3"}]];pt(e,gt({name:"settings"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Dd(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"}]];pt(e,gt({name:"shield"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function Ud(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"4"}],["path",{d:"M12 2v2"}],["path",{d:"M12 20v2"}],["path",{d:"m4.93 4.93 1.41 1.41"}],["path",{d:"m17.66 17.66 1.41 1.41"}],["path",{d:"M2 12h2"}],["path",{d:"M20 12h2"}],["path",{d:"m6.34 17.66-1.41 1.41"}],["path",{d:"m19.07 4.93-1.41 1.41"}]];pt(e,gt({name:"sun"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}function zd(e,t){Te(t,!0);/**
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
 */let r=ut(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M4 14a1 1 0 0 1-.78-1.63l9.9-10.2a.5.5 0 0 1 .86.46l-1.92 6.02A1 1 0 0 0 13 10h7a1 1 0 0 1 .78 1.63l-9.9 10.2a.5.5 0 0 1-.86-.46l1.92-6.02A1 1 0 0 0 11 14z"}]];pt(e,gt({name:"zap"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=Re(),d=xe(l);ct(d,()=>t.children??Ce),g(o,l)},$$slots:{default:!0}})),Pe()}var Bd=x('<p class="text-sm text-red-500 dark:text-red-400"> </p>'),Wd=x('<div class="flex min-h-screen items-center justify-center bg-gray-50 px-4 py-8 text-gray-900 dark:bg-gray-900 dark:text-gray-100"><div class="w-full max-w-md rounded-xl border border-gray-200 bg-white p-6 shadow-xl shadow-black/10 dark:border-gray-700 dark:bg-gray-800 dark:shadow-black/30"><div class="flex items-center justify-between gap-3"><h1 class="text-2xl font-semibold tracking-tight"> </h1> <button type="button" class="rounded-lg border border-gray-300 bg-gray-50 px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <p class="mt-2 text-sm text-gray-500 dark:text-gray-400"> </p> <form class="mt-6 space-y-4"><label class="block text-sm font-medium text-gray-600 dark:text-gray-300" for="token"> </label> <input id="token" type="password" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-gray-900 outline-none ring-sky-500 transition focus:ring-2 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100" autocomplete="off"/> <!> <button type="submit" class="w-full rounded-lg bg-sky-600 px-4 py-2 font-medium text-white transition hover:bg-sky-500"> </button></form></div></div>');function qd(e,t){Te(t,!0);let r=R(""),n=R("");function o(N){var W;N.preventDefault();const P=a(r).trim();if(!P){v(n,h("login.tokenRequired"),!0);return}Ol(P),v(n,""),(W=t.onLogin)==null||W.call(t,P)}var s=Wd(),l=i(s),d=i(l),c=i(d),f=i(c),_=p(c,2),w=i(_),k=p(d,2),I=i(k),T=p(k,2),j=i(T),C=i(j),O=p(j,2),q=p(O,2);{var K=N=>{var P=Bd(),W=i(P);M(()=>y(W,a(n))),g(N,P)};z(q,N=>{a(n)&&N(K)})}var S=p(q,2),m=i(S);M((N,P,W,Se,_e,je)=>{y(f,N),$e(_,"aria-label",P),y(w,Qr.lang==="zh"?"中文 / EN":"EN / 中文"),y(I,W),y(C,Se),$e(O,"placeholder",_e),y(m,je)},[()=>h("login.title"),()=>h("app.language"),()=>h("login.hint"),()=>h("login.accessToken"),()=>h("login.placeholder"),()=>h("login.login")]),te("click",_,function(...N){na==null||na.apply(this,N)}),xr("submit",T,o),Br(O,()=>a(r),N=>v(r,N)),g(e,s),Pe()}ir(["click"]);function Vd(e){if(!Number.isFinite(e)||e<0)return"0s";const t=Math.floor(e/86400),r=Math.floor(e%86400/3600),n=Math.floor(e%3600/60),o=Math.floor(e%60),s=[];return t>0&&s.push(`${t}d`),(r>0||s.length>0)&&s.push(`${r}h`),(n>0||s.length>0)&&s.push(`${n}m`),s.push(`${o}s`),s.join(" ")}var Gd=x('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),Kd=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Jd=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Yd=x('<div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><p class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400"> </p> <p class="mt-2 text-lg font-semibold text-gray-900 dark:text-gray-100"> </p></div>'),Xd=x('<p class="mt-3 text-sm text-gray-500 dark:text-gray-400"> </p>'),Qd=x('<li class="rounded-full border border-gray-300 bg-gray-50 px-3 py-1 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"> </li>'),Zd=x('<ul class="mt-3 flex flex-wrap gap-2"></ul>'),ec=x('<div class="grid gap-4 sm:grid-cols-2 xl:grid-cols-5"></div> <div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><h3 class="text-sm font-semibold uppercase tracking-wide text-gray-600 dark:text-gray-300"> </h3> <!></div>',1),tc=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function rc(e,t){Te(t,!0);let r=R(null),n=R(!0),o=R(""),s=R("");function l(m){return typeof m!="string"||m.length===0?h("common.unknown"):m.replaceAll("_"," ").split(" ").map(N=>N.charAt(0).toUpperCase()+N.slice(1)).join(" ")}function d(m){const N=`channels.names.${m}`,P=h(N);return P===N?l(m):P}const c=ce(()=>{var m,N,P,W,Se;return[{label:h("overview.version"),value:((m=a(r))==null?void 0:m.version)??h("common.na")},{label:h("overview.uptime"),value:typeof((N=a(r))==null?void 0:N.uptime_seconds)=="number"?Vd(a(r).uptime_seconds):h("common.na")},{label:h("overview.model"),value:((P=a(r))==null?void 0:P.model)??h("common.na")},{label:h("overview.memoryBackend"),value:((W=a(r))==null?void 0:W.memory_backend)??h("common.na")},{label:h("overview.gatewayPort"),value:(Se=a(r))!=null&&Se.gateway_port?String(a(r).gateway_port):h("common.na")}]}),f=ce(()=>{var m;return Array.isArray((m=a(r))==null?void 0:m.channels)?a(r).channels:[]});async function _(){try{const m=await ht.getStatus();v(r,m,!0),v(o,""),v(s,new Date().toLocaleTimeString(),!0)}catch(m){v(o,m instanceof Error?m.message:h("overview.loadFailed"),!0)}finally{v(n,!1)}}Lt(()=>{let m=!1;const N=async()=>{m||await _()};N();const P=setInterval(N,3e4);return()=>{m=!0,clearInterval(P)}});var w=tc(),k=i(w),I=i(k),T=i(I),j=p(I,2);{var C=m=>{var N=Gd(),P=i(N);M(W=>y(P,W),[()=>h("common.updatedAt",{time:a(s)})]),g(m,N)};z(j,m=>{a(s)&&m(C)})}var O=p(k,2);{var q=m=>{var N=Kd(),P=i(N);M(W=>y(P,W),[()=>h("overview.loading")]),g(m,N)},K=m=>{var N=Jd(),P=i(N);M(()=>y(P,a(o))),g(m,N)},S=m=>{var N=ec(),P=xe(N);Xe(P,21,()=>a(c),rt,(Z,ee)=>{var oe=Yd(),Oe=i(oe),ze=i(Oe),ke=p(Oe,2),He=i(ke);M(()=>{y(ze,a(ee).label),y(He,a(ee).value)}),g(Z,oe)});var W=p(P,2),Se=i(W),_e=i(Se),je=p(Se,2);{var qe=Z=>{var ee=Xd(),oe=i(ee);M(Oe=>y(oe,Oe),[()=>h("overview.noChannelsConfigured")]),g(Z,ee)},B=Z=>{var ee=Zd();Xe(ee,21,()=>a(f),rt,(oe,Oe)=>{var ze=Qd(),ke=i(ze);M(He=>y(ke,He),[()=>d(a(Oe))]),g(oe,ze)}),g(Z,ee)};z(je,Z=>{a(f).length===0?Z(qe):Z(B,-1)})}M(Z=>y(_e,Z),[()=>h("overview.configuredChannels")]),g(m,N)};z(O,m=>{a(n)?m(q):a(o)?m(K,1):m(S,-1)})}M(m=>y(T,m),[()=>h("overview.title")]),g(e,w),Pe()}var ac=x('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),nc=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),oc=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),sc=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),ic=x('<tr class="cursor-pointer transition hover:bg-gray-50 dark:hover:bg-gray-700/40"><td class="px-4 py-3 font-mono text-xs"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td></tr>'),lc=x('<div class="overflow-x-auto rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><table class="min-w-full divide-y divide-gray-200 text-sm dark:divide-gray-700"><thead class="bg-gray-50 text-left text-gray-600 dark:bg-gray-900/50 dark:text-gray-300"><tr><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th></tr></thead><tbody class="divide-y divide-gray-200 text-gray-700 dark:divide-gray-700 dark:text-gray-200"></tbody></table></div>'),dc=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function cc(e,t){Te(t,!0);let r=R(mt([])),n=R(!0),o=R(""),s=R("");function l(m){return typeof m!="string"||m.length===0?h("common.unknown"):m.replaceAll("_"," ").split(" ").map(N=>N.charAt(0).toUpperCase()+N.slice(1)).join(" ")}function d(m){const N=`channels.names.${m}`,P=h(N);return P===N?l(m):P}async function c(){try{const m=await ht.getSessions();v(r,Array.isArray(m)?m:[],!0),v(o,""),v(s,new Date().toLocaleTimeString(),!0)}catch(m){v(o,m instanceof Error?m.message:h("sessions.loadFailed"),!0)}finally{v(n,!1)}}function f(m){$r(`/chat/${encodeURIComponent(m)}`)}Lt(()=>{let m=!1;const N=async()=>{m||await c()};N();const P=setInterval(N,15e3);return()=>{m=!0,clearInterval(P)}});var _=dc(),w=i(_),k=i(w),I=i(k),T=p(k,2);{var j=m=>{var N=ac(),P=i(N);M(W=>y(P,W),[()=>h("common.updatedAt",{time:a(s)})]),g(m,N)};z(T,m=>{a(s)&&m(j)})}var C=p(w,2);{var O=m=>{var N=nc(),P=i(N);M(W=>y(P,W),[()=>h("sessions.loading")]),g(m,N)},q=m=>{var N=oc(),P=i(N);M(()=>y(P,a(o))),g(m,N)},K=m=>{var N=sc(),P=i(N);M(W=>y(P,W),[()=>h("sessions.none")]),g(m,N)},S=m=>{var N=lc(),P=i(N),W=i(P),Se=i(W),_e=i(Se),je=i(_e),qe=p(_e),B=i(qe),Z=p(qe),ee=i(Z),oe=p(Z),Oe=i(oe),ze=p(oe),ke=i(ze),He=p(W);Xe(He,21,()=>a(r),rt,(yt,be)=>{var ne=ic(),we=i(ne),D=i(we),J=p(we),Ye=i(J),ot=p(J),_t=i(ot),H=p(ot),U=i(H),me=p(H),st=i(me);M((lt,et)=>{y(D,a(be).session_id),y(Ye,a(be).sender),y(_t,lt),y(U,a(be).message_count),y(st,et)},[()=>d(a(be).channel),()=>a(be).last_message_preview||h("common.empty")]),te("click",ne,()=>f(a(be).session_id)),g(yt,ne)}),M((yt,be,ne,we,D)=>{y(je,yt),y(B,be),y(ee,ne),y(Oe,we),y(ke,D)},[()=>h("sessions.sessionId"),()=>h("sessions.sender"),()=>h("sessions.channel"),()=>h("sessions.messages"),()=>h("sessions.lastMessage")]),g(m,N)};z(C,m=>{a(n)?m(O):a(o)?m(q,1):a(r).length===0?m(K,2):m(S,-1)})}M(m=>y(I,m),[()=>h("sessions.title")]),g(e,_),Pe()}ir(["click"]);var uc=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),fc=x('<p class="mb-3 rounded-lg border border-blue-500/40 bg-blue-500/15 px-3 py-2 text-sm text-blue-700 dark:text-blue-200"> </p>'),vc=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),gc=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),pc=x('<p class="whitespace-pre-wrap break-words text-sm"> </p>'),yc=x('<img alt="Attachment" class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 object-contain dark:border-gray-600/40" loading="lazy"/>'),bc=x('<video controls="" class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 dark:border-gray-600/40"></video>',2),hc=x("<div></div>"),mc=x('<div class="space-y-3"></div>'),_c=x('<img class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"/>'),xc=x('<video class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"></video>',2),kc=x('<div class="flex h-12 w-12 items-center justify-center rounded border border-gray-300 bg-gray-100 text-lg text-gray-700 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200">DOC</div>'),wc=x('<div class="flex items-center gap-2 rounded-md border border-gray-200 bg-white/90 p-2 dark:border-gray-700 dark:bg-gray-800/90"><!> <div class="min-w-0 flex-1"><p class="truncate text-sm text-gray-900 dark:text-gray-100"> </p> <p class="truncate text-xs text-gray-500 dark:text-gray-400"> </p></div> <button type="button" class="rounded px-2 py-1 text-xs text-gray-600 hover:bg-gray-200 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-white">Remove</button></div>'),Sc=x('<div class="mb-3 space-y-2 rounded-lg border border-gray-200 bg-gray-50/70 p-2.5 dark:border-gray-700 dark:bg-gray-900/70"><p class="text-xs text-gray-600 dark:text-gray-300"> </p> <div class="max-h-44 space-y-2 overflow-y-auto pr-1"></div></div>'),Ec=x('<section class="flex h-[calc(100vh-10rem)] flex-col gap-4"><div class="flex items-center justify-between"><div class="min-w-0"><h2 class="text-2xl font-semibold"> </h2> <p class="truncate font-mono text-xs text-gray-500 dark:text-gray-400"> </p></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!> <div class="flex min-h-0 flex-1 flex-col rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800" role="region" aria-label="Chat messages"><div><!> <!></div> <form class="border-t border-gray-200 p-3 dark:border-gray-700"><input type="file" class="hidden" multiple="" accept="image/*,video/*,.pdf,.doc,.docx,.txt,.md,.csv,.json,.zip,.tar,.gz,.rar,.ppt,.pptx,.xls,.xlsx"/> <!> <div class="flex items-end gap-2"><textarea rows="2" class="min-h-[2.75rem] flex-1 resize-y rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-900 outline-none focus:border-blue-500 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100"></textarea> <button type="button" title="Attach files" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-600 hover:border-gray-400 hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:border-gray-500 dark:hover:bg-gray-700"><!></button> <button type="submit" class="rounded-lg bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"> </button></div></form></div></section>');function Ac(e,t){Te(t,!0);const r=10,n=/\[(IMAGE|VIDEO):([^\]]+)\]|(data:(?:image|video)\/[a-zA-Z0-9.+-]+;base64,[a-zA-Z0-9+/=]+)/gi;let o=aa(t,"sessionId",3,""),s=R(mt([])),l=R(""),d=R(!0),c=R(!1),f=R(""),_=R(null),w=R(null),k=R(mt([])),I=R(!1),T=0;function j(){$r("/sessions")}function C(A){return A==="user"?"ml-auto max-w-[85%] rounded-2xl rounded-br-md bg-blue-600 px-4 py-2 text-white":A==="assistant"?"mr-auto max-w-[85%] rounded-2xl rounded-bl-md bg-gray-200 px-4 py-2 text-gray-900 dark:bg-gray-700 dark:text-gray-100":"mx-auto max-w-[90%] rounded-lg bg-gray-100/60 px-3 py-1.5 text-center text-xs text-gray-500 dark:bg-gray-800/60 dark:text-gray-400"}function O(A){return((A==null?void 0:A.type)||"").startsWith("image/")}function q(A){return((A==null?void 0:A.type)||"").startsWith("video/")}function K(A){if(!Number.isFinite(A)||A<=0)return"0 B";const G=["B","KB","MB","GB"];let u=A,b=0;for(;u>=1024&&b<G.length-1;)u/=1024,b+=1;return`${u.toFixed(b===0?0:1)} ${G[b]}`}function S(A){return typeof A=="string"&&A.trim().length>0?A:"unknown"}function m(A){const G=O(A),u=q(A);return{id:`${A.name}-${A.lastModified}-${Math.random().toString(36).slice(2)}`,file:A,name:A.name,size:A.size,type:S(A.type),isImage:G,isVideo:u,previewUrl:G||u?URL.createObjectURL(A):""}}function N(A){A&&typeof A.previewUrl=="string"&&A.previewUrl.startsWith("blob:")&&URL.revokeObjectURL(A.previewUrl)}function P(){for(const A of a(k))N(A);v(k,[],!0),a(w)&&(a(w).value="")}function W(A){if(!A||A.length===0||a(c))return;const G=Array.from(A),u=[],b=Math.max(0,r-a(k).length);for(const E of G.slice(0,b))u.push(m(E));v(k,[...a(k),...u],!0)}function Se(A){const G=a(k).find(u=>u.id===A);G&&N(G),v(k,a(k).filter(u=>u.id!==A),!0)}function _e(){var A;a(c)||(A=a(w))==null||A.click()}function je(A){var G;W((G=A.currentTarget)==null?void 0:G.files),a(w)&&(a(w).value="")}function qe(A){A.preventDefault(),!a(c)&&(T+=1,v(I,!0))}function B(A){A.preventDefault(),!a(c)&&A.dataTransfer&&(A.dataTransfer.dropEffect="copy")}function Z(A){A.preventDefault(),T=Math.max(0,T-1),T===0&&v(I,!1)}function ee(A){var G;A.preventDefault(),T=0,v(I,!1),W((G=A.dataTransfer)==null?void 0:G.files)}function oe(A){const G=(A||"").trim();if(!G)return"";const u=G.toLowerCase();return u.startsWith("data:image/")||u.startsWith("data:video/")||u.startsWith("http://")||u.startsWith("https://")?G:ht.getSessionMediaUrl(G)}function Oe(A,G){const u=(G||"").trim().toLowerCase();return A==="VIDEO"||u.startsWith("data:video/")?"video":u.startsWith("data:image/")?"image":[".mp4",".webm",".mov",".m4v",".ogg"].some(E=>u.endsWith(E))?"video":"image"}function ze(A){if(typeof A!="string"||A.length===0)return[];const G=[];n.lastIndex=0;let u=0,b;for(;(b=n.exec(A))!==null;){b.index>u&&G.push({id:`text-${u}`,kind:"text",value:A.slice(u,b.index)});const E=(b[1]||"").toUpperCase(),F=(b[2]||b[3]||"").trim();if(F){const L=Oe(E,F);G.push({id:`${L}-${b.index}`,kind:L,value:F})}u=n.lastIndex}return u<A.length&&G.push({id:`text-tail-${u}`,kind:"text",value:A.slice(u)}),G}async function ke(){await ys(),a(_)&&(a(_).scrollTop=a(_).scrollHeight)}async function He(){try{const A=await ht.getSessionMessages(o());v(s,Array.isArray(A)?A:[],!0),v(f,""),await ke()}catch(A){v(f,A instanceof Error?A.message:h("chat.loadFailed"),!0)}finally{v(d,!1)}}async function yt(){const A=a(l).trim(),G=a(k).map(b=>b.file);if(A.length===0&&G.length===0||a(c))return;v(c,!0),v(l,""),v(f,"");const u=G.length>0;u||(v(s,[...a(s),{role:"user",content:A}],!0),await ke());try{const b=u?await ht.sendMessageWithMedia(o(),A,G):await ht.sendMessage(o(),A);u?await He():b&&typeof b.reply=="string"&&b.reply.length>0&&v(s,[...a(s),{role:"assistant",content:b.reply}],!0),P()}catch(b){v(f,b instanceof Error?b.message:h("chat.sendFailed"),!0),await He()}finally{v(c,!1),await ke()}}function be(A){A.preventDefault(),yt()}Lt(()=>{let A=!1;return(async()=>{A||(v(d,!0),await He())})(),()=>{A=!0}}),Nl(()=>{for(const A of a(k))N(A)});var ne=Ec(),we=i(ne),D=i(we),J=i(D),Ye=i(J),ot=p(J,2),_t=i(ot),H=p(D,2),U=i(H),me=p(we,2);{var st=A=>{var G=uc(),u=i(G);M(()=>y(u,a(f))),g(A,G)};z(me,A=>{a(f)&&A(st)})}var lt=p(me,2),et=i(lt),tt=i(et);{var De=A=>{var G=fc(),u=i(G);M(()=>y(u,`Drop files to attach (${a(k).length??""}/10 selected)`)),g(A,G)};z(tt,A=>{a(I)&&A(De)})}var Ge=p(tt,2);{var at=A=>{var G=vc(),u=i(G);M(b=>y(u,b),[()=>h("chat.loading")]),g(A,G)},Be=A=>{var G=gc(),u=i(G);M(b=>y(u,b),[()=>h("chat.empty")]),g(A,G)},St=A=>{var G=mc();Xe(G,21,()=>a(s),rt,(u,b)=>{var E=hc();Xe(E,21,()=>ze(a(b).content),F=>F.id,(F,L)=>{var V=Re(),se=xe(V);{var le=ae=>{var Y=Re(),fe=xe(Y);{var X=ie=>{var de=pc(),pe=i(de);M(()=>y(pe,a(L).value)),g(ie,de)},re=ce(()=>a(L).value.trim().length>0);z(fe,ie=>{a(re)&&ie(X)})}g(ae,Y)},ve=ae=>{var Y=yc();M(fe=>$e(Y,"src",fe),[()=>oe(a(L).value)]),g(ae,Y)},Ie=ae=>{var Y=bc();M(fe=>$e(Y,"src",fe),[()=>oe(a(L).value)]),g(ae,Y)};z(se,ae=>{a(L).kind==="text"?ae(le):a(L).kind==="image"?ae(ve,1):a(L).kind==="video"&&ae(Ie,2)})}g(F,V)}),M(F=>Qe(E,1,F),[()=>eo(C(a(b).role))]),g(u,E)}),g(A,G)};z(Ge,A=>{a(d)?A(at):a(s).length===0?A(Be,1):A(St,-1)})}jn(et,A=>v(_,A),()=>a(_));var ge=p(et,2),ue=i(ge);jn(ue,A=>v(w,A),()=>a(w));var Ee=p(ue,2);{var Ze=A=>{var G=Sc(),u=i(G),b=i(u),E=p(u,2);Xe(E,21,()=>a(k),F=>F.id,(F,L)=>{var V=wc(),se=i(V);{var le=de=>{var pe=_c();M(()=>{$e(pe,"src",a(L).previewUrl),$e(pe,"alt",a(L).name)}),g(de,pe)},ve=de=>{var pe=xc();pe.muted=!0,M(()=>$e(pe,"src",a(L).previewUrl)),g(de,pe)},Ie=de=>{var pe=kc();g(de,pe)};z(se,de=>{a(L).isImage?de(le):a(L).isVideo?de(ve,1):de(Ie,-1)})}var ae=p(se,2),Y=i(ae),fe=i(Y),X=p(Y,2),re=i(X),ie=p(ae,2);M(de=>{y(fe,a(L).name),y(re,`${a(L).type??""} · ${de??""}`)},[()=>K(a(L).size)]),te("click",ie,()=>Se(a(L).id)),g(F,V)}),M(()=>y(b,`Attachments (${a(k).length??""}/10)`)),g(A,G)};z(Ee,A=>{a(k).length>0&&A(Ze)})}var Ae=p(Ee,2),it=i(Ae),ft=p(it,2),nt=i(ft);Rd(nt,{size:16});var bt=p(ft,2),Et=i(bt);M((A,G,u,b,E,F)=>{y(Ye,A),y(_t,`${G??""}: ${o()??""}`),y(U,u),Qe(et,1,`flex-1 overflow-y-auto p-4 ${a(I)?"bg-blue-500/10 ring-1 ring-inset ring-blue-500/40":""}`),$e(it,"placeholder",b),ft.disabled=a(c)||a(k).length>=r,bt.disabled=E,y(Et,F)},[()=>h("chat.title"),()=>h("chat.session"),()=>h("chat.back"),()=>h("chat.inputPlaceholder"),()=>a(c)||!a(l).trim()&&a(k).length===0,()=>a(c)?h("chat.sending"):h("chat.send")]),te("click",H,j),xr("dragenter",lt,qe),xr("dragover",lt,B),xr("dragleave",lt,Z),xr("drop",lt,ee),xr("submit",ge,be),te("change",ue,je),Br(it,()=>a(l),A=>v(l,A)),te("click",ft,_e),g(e,ne),Pe()}ir(["click","change"]);var $c=x('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),Cc=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Mc=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Nc=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Tc=x('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-3 text-sm text-gray-500 dark:text-gray-400"> </p> <p class="mt-1 text-sm text-gray-500 dark:text-gray-400"> </p></article>'),Pc=x('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),Oc=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function Fc(e,t){Te(t,!0);let r=R(mt([])),n=R(!0),o=R(""),s=R("");function l(S){return typeof S!="string"||S.length===0?h("common.unknown"):S.replaceAll("_"," ").split(" ").map(m=>m.charAt(0).toUpperCase()+m.slice(1)).join(" ")}function d(S){const m=`channels.names.${S}`,N=h(m);return N===m?l(S):N}async function c(){try{const S=await ht.getChannelsStatus();v(r,Array.isArray(S==null?void 0:S.channels)?S.channels:[],!0),v(o,""),v(s,new Date().toLocaleTimeString(),!0)}catch(S){v(o,S instanceof Error?S.message:h("channels.loadFailed"),!0)}finally{v(n,!1)}}Lt(()=>{let S=!1;const m=async()=>{S||await c()};m();const N=setInterval(m,3e4);return()=>{S=!0,clearInterval(N)}});var f=Oc(),_=i(f),w=i(_),k=i(w),I=p(w,2);{var T=S=>{var m=$c(),N=i(m);M(P=>y(N,P),[()=>h("common.updatedAt",{time:a(s)})]),g(S,m)};z(I,S=>{a(s)&&S(T)})}var j=p(_,2);{var C=S=>{var m=Cc(),N=i(m);M(P=>y(N,P),[()=>h("channels.loading")]),g(S,m)},O=S=>{var m=Mc(),N=i(m);M(()=>y(N,a(o))),g(S,m)},q=S=>{var m=Nc(),N=i(m);M(P=>y(N,P),[()=>h("channels.noChannels")]),g(S,m)},K=S=>{var m=Pc();Xe(m,21,()=>a(r),rt,(N,P)=>{var W=Tc(),Se=i(W),_e=i(Se),je=i(_e),qe=p(_e,2),B=i(qe),Z=p(Se,2),ee=i(Z),oe=p(Z,2),Oe=i(oe);M((ze,ke,He,yt,be,ne)=>{y(je,ze),Qe(qe,1,`rounded-full px-2 py-1 text-xs font-medium ${a(P).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),y(B,ke),y(ee,`${He??""}: ${yt??""}`),y(Oe,`${be??""}: ${ne??""}`)},[()=>d(a(P).name),()=>a(P).enabled?h("common.enabled"):h("common.disabled"),()=>h("channels.type"),()=>d(a(P).type),()=>h("channels.status"),()=>d(a(P).status)]),g(N,W)}),g(S,m)};z(j,S=>{a(n)?S(C):a(o)?S(O,1):a(r).length===0?S(q,2):S(K,-1)})}M(S=>y(k,S),[()=>h("channels.title")]),g(e,f),Pe()}function kn(e){return e.replaceAll("&","&amp;").replaceAll("<","&lt;").replaceAll(">","&gt;").replaceAll('"',"&quot;")}const Eo=/(\"(\\u[0-9a-fA-F]{4}|\\[^u]|[^\\\"])*\"(?:\s*:)?|\btrue\b|\bfalse\b|\bnull\b|-?\d+(?:\.\d+)?(?:[eE][+\-]?\d+)?)/g;function Ic(e){return e.startsWith('"')?e.endsWith(":")?"text-sky-300":"text-emerald-300":e==="true"||e==="false"?"text-amber-300":e==="null"?"text-fuchsia-300":"text-violet-300"}function Lc(e){if(!e)return"";let t="",r=0;Eo.lastIndex=0;for(const n of e.matchAll(Eo)){const o=n.index??0,s=n[0];t+=kn(e.slice(r,o)),t+=`<span class="${Ic(s)}">${kn(s)}</span>`,r=o+s.length}return t+=kn(e.slice(r)),t}var Rc=x('<span class="ml-1.5 text-xs text-sky-500 dark:text-sky-400">已修改</span>'),jc=x('<button type="button"><span></span></button>'),Hc=x("<option> </option>"),Dc=x('<select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select>'),Uc=x('<input type="number" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/>'),zc=x('<div class="flex gap-1"><input type="text" class="flex-1 rounded border border-gray-300 bg-white px-2 py-1 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <button type="button" class="rounded border border-gray-300 bg-white px-2 py-1 text-xs text-red-500 hover:bg-red-500/10 dark:border-gray-600 dark:bg-gray-800 dark:text-red-400">×</button></div>'),Bc=x('<div class="space-y-1.5"><!> <button type="button" class="rounded border border-dashed border-gray-300 px-2 py-1 text-xs text-gray-500 hover:border-sky-500 hover:text-sky-500 dark:border-gray-600 dark:text-gray-400 dark:hover:border-sky-500 dark:hover:text-sky-400">+ 添加</button></div>'),Wc=x('<div class="flex gap-1"><input class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <button type="button" class="rounded-lg border border-gray-300 bg-white px-2 text-xs text-gray-500 hover:text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-400 dark:hover:text-gray-200"> </button></div>'),qc=x('<input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/>'),Vc=x('<div><div class="flex items-start justify-between gap-3"><div class="flex-1 min-w-0"><label class="block text-sm font-medium text-gray-700 dark:text-gray-200"> <!></label> <p class="mt-0.5 text-xs text-gray-400 dark:text-gray-500"> </p></div> <div class="flex-shrink-0 w-64"><!></div></div></div>'),Gc=x('<button type="button"><span></span></button>'),Kc=x('<input type="number" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/>'),Jc=x('<div class="flex gap-1"><input type="text" class="flex-1 rounded border border-gray-300 bg-white px-2 py-1 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <button type="button" class="rounded border border-gray-300 px-2 py-1 text-xs text-red-500 hover:bg-red-500/10 dark:border-gray-600 dark:bg-gray-800">×</button></div>'),Yc=x('<div class="space-y-1.5"><!> <button type="button" class="rounded border border-dashed border-gray-300 px-2 py-1 text-xs text-gray-500 hover:border-sky-500 hover:text-sky-500 dark:border-gray-600">+ 添加</button></div>'),Xc=x('<div class="flex gap-1"><input class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200" placeholder="未设置"/> <button type="button" class="rounded-lg border border-gray-300 bg-white px-2 text-xs text-gray-500 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-400"> </button></div>'),Qc=x('<input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200" placeholder="未设置"/>'),Zc=x('<textarea class="w-full rounded-lg border border-gray-300 bg-white font-mono text-xs leading-relaxed p-2 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 resize-y"></textarea>'),eu=x('<span class="text-xs text-sky-500">已修改</span>'),tu=x('<div class="mb-2 flex items-center gap-2"><!> <span class="font-mono text-xs font-medium text-gray-600 dark:text-gray-300"> </span> <!></div> <!>',1),ru=x('<span class="ml-1.5 text-xs text-sky-500">已修改</span>'),au=x('<div class="flex items-center justify-between gap-3"><span class="min-w-0 flex-1 font-mono text-sm text-gray-700 dark:text-gray-200"> <!></span> <div class="w-56 flex-shrink-0"><!></div></div>'),nu=x("<div><!></div>"),ou=x('<span class="inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),su=x('<span class="ml-auto text-xs text-gray-400"> </span>'),iu=x('<details class="rounded-lg border border-gray-200 dark:border-gray-700"><summary class="cursor-pointer select-none flex items-center gap-2 px-3 py-2 text-sm font-medium text-gray-700 dark:text-gray-200 hover:bg-gray-50 dark:hover:bg-gray-700/50 rounded-lg"><span class="font-mono"> </span> <!> <!></summary> <div class="border-t border-gray-200 px-3 py-2 space-y-2 dark:border-gray-700"><!></div></details>'),lu=x('<div class="space-y-2"></div>'),du=x('<p class="text-sm text-gray-500 dark:text-gray-400">加载配置中...</p>'),cu=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),uu=x('<div class="overflow-x-auto rounded-xl border border-gray-200 bg-gray-50 p-4 dark:border-gray-700 dark:bg-gray-950"><pre class="text-sm leading-6 text-gray-700 dark:text-gray-200"><code><!></code></pre></div>'),fu=x('<span class="rounded-full bg-gray-100 px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-gray-500 dark:bg-gray-700 dark:text-gray-300">Auto</span>'),vu=x('<span class="inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),gu=x('<button type="button"><span> </span> <!> <!></button>'),pu=x('<span class="ml-2 inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),yu=x('<div class="mt-2 border-t border-gray-100 pt-3 dark:border-gray-700/60"><p class="mb-2 text-xs font-medium uppercase tracking-wider text-gray-400 dark:text-gray-500">其他子配置</p> <div class="space-y-2"></div></div>'),bu=x('<details class="group scroll-mt-24 rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><summary class="cursor-pointer select-none px-4 py-3 text-base font-semibold text-gray-900 flex items-center gap-2 dark:text-gray-100"><!> <span> </span> <!></summary> <div class="border-t border-gray-200 px-4 py-3 space-y-3 dark:border-gray-700"><!> <!></div></details>'),hu=x('<span class="inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),mu=x('<details class="scroll-mt-24 rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><summary class="cursor-pointer select-none px-4 py-3 flex items-center gap-2 dark:text-gray-100"><!> <span class="font-mono text-sm font-semibold text-gray-800 dark:text-gray-100"> </span> <!> <span class="ml-auto text-xs text-gray-400 dark:text-gray-500"> </span></summary> <div class="border-t border-gray-200 px-4 py-3 dark:border-gray-700"><!></div></details>'),_u=x('<div class="pt-1"><p class="mb-2 px-1 text-xs font-semibold uppercase tracking-wider text-gray-400 dark:text-gray-500">自动发现的配置项</p> <div class="space-y-3"></div></div>'),xu=x('<div class="space-y-3"><div class="sticky top-0 z-20 -mx-1 overflow-x-auto rounded-xl border border-gray-200 bg-white/95 px-3 py-3 backdrop-blur dark:border-gray-700 dark:bg-gray-900/95"><div class="flex min-w-max items-center gap-2"></div></div> <!> <!></div>'),ku=x('<div class="flex items-start gap-2 text-xs flex-wrap"><span class="flex-shrink-0 text-gray-400 dark:text-gray-500"> </span> <span class="font-medium text-gray-600 dark:text-gray-300"> </span> <span class="text-red-500 line-through dark:text-red-400 break-all"> </span> <span class="text-gray-400 dark:text-gray-600">→</span> <span class="text-green-600 dark:text-green-400 break-all"> </span></div>'),wu=x('<div class="mx-auto mt-3 max-w-5xl rounded-lg border border-gray-200 bg-gray-50 p-3 dark:border-gray-700 dark:bg-gray-950"><p class="mb-2 text-xs font-medium text-gray-500 dark:text-gray-400">变更详情</p> <div class="space-y-1.5 max-h-48 overflow-y-auto"></div></div>'),Su=x('<div class="fixed bottom-0 left-0 right-0 z-50 border-t border-gray-200 bg-white/95 px-6 py-3 backdrop-blur-sm dark:border-gray-700 dark:bg-gray-900/95"><div class="mx-auto flex max-w-5xl items-center justify-between gap-4"><div class="flex items-center gap-3"><span class="text-sm text-sky-600 dark:text-sky-400"> </span> <button type="button" class="text-sm text-gray-500 underline hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"> </button></div> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-4 py-2 text-sm text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700">放弃修改</button> <button type="button" class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"> </button></div></div> <!></div>'),Eu=x("<div> </div>"),Au=x('<section class="space-y-4 pb-24"><div class="flex items-center justify-between gap-4"><h2 class="text-2xl font-semibold"> </h2> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700">复制 JSON</button></div></div> <!> <!> <!></section>');function $u(e,t){Te(t,!0);const r=(u,b=Ce,E=Ce)=>{const F=ce(()=>B(b())),L=ce(()=>a(He).has(b())),V=ce(()=>a(O).has(b()));var se=Vc(),le=i(se),ve=i(le),Ie=i(ve),ae=i(Ie),Y=p(ae);{var fe=Le=>{var ye=Rc();g(Le,ye)};z(Y,Le=>{a(L)&&Le(fe)})}var X=p(Ie,2),re=i(X),ie=p(ve,2),de=i(ie);{var pe=Le=>{var ye=jc(),Ke=i(ye);M(()=>{Qe(ye,1,`relative inline-flex h-6 w-11 items-center rounded-full transition-colors ${a(F)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Qe(Ke,1,`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${a(F)?"translate-x-6":"translate-x-1"}`)}),te("click",ye,()=>J(b(),!a(F))),g(Le,ye)},Q=Le=>{var ye=Dc();Xe(ye,21,()=>E().options,rt,(dt,It)=>{var Ve=Hc(),At=i(Ve),Ht={};M(()=>{y(At,a(It)||"(默认)"),Ht!==(Ht=a(It))&&(Ve.value=(Ve.__value=a(It))??"")}),g(dt,Ve)});var Ke;to(ye),M(()=>{Ke!==(Ke=a(F)??E().default)&&(ye.value=(ye.__value=a(F)??E().default)??"",La(ye,a(F)??E().default))}),te("change",ye,dt=>J(b(),dt.target.value)),g(Le,ye)},he=Le=>{var ye=Uc();M(Ke=>{hr(ye,a(F)??E().default),$e(ye,"min",E().min),$e(ye,"max",E().max),$e(ye,"step",E().step??1),$e(ye,"placeholder",Ke)},[()=>String(E().default)]),te("input",ye,Ke=>{const dt=E().step&&E().step<1?parseFloat(Ke.target.value):parseInt(Ke.target.value,10);isNaN(dt)||J(b(),dt)}),g(Le,ye)},Ne=Le=>{var ye=Bc(),Ke=i(ye);{var dt=At=>{var Ht=Re(),Er=xe(Ht);Xe(Er,17,()=>a(F),rt,(Rt,Ba,Ea)=>{var ea=zc(),ta=i(ea),fn=p(ta,2);M(()=>hr(ta,a(Ba))),te("input",ta,vn=>_t(b(),Ea,vn.target.value)),te("click",fn,()=>ot(b(),Ea)),g(Rt,ea)}),g(At,Ht)},It=ce(()=>Array.isArray(a(F)));z(Ke,At=>{a(It)&&At(dt)})}var Ve=p(Ke,2);te("click",Ve,()=>Ye(b())),g(Le,ye)},vt=Le=>{var ye=Wc(),Ke=i(ye),dt=p(Ke,2),It=i(dt);M(()=>{$e(Ke,"type",a(V)?"text":"password"),hr(Ke,a(F)??""),$e(Ke,"placeholder",E().default||"未设置"),y(It,a(V)?"隐藏":"显示")}),te("input",Ke,Ve=>J(b(),Ve.target.value)),te("click",dt,()=>H(b())),g(Le,ye)},xt=Le=>{var ye=qc();M(()=>{hr(ye,a(F)??""),$e(ye,"placeholder",E().default||"未设置")}),te("input",ye,Ke=>J(b(),Ke.target.value)),g(Le,ye)};z(de,Le=>{E().type==="bool"?Le(pe):E().type==="enum"?Le(Q,1):E().type==="number"?Le(he,2):E().type==="array"?Le(Ne,3):E().sensitive?Le(vt,4):Le(xt,-1)})}M(()=>{Qe(se,1,`rounded-lg border p-3 transition-colors ${a(L)?"border-sky-500/50 bg-sky-500/5":"border-gray-200 bg-gray-50/40 dark:border-gray-700 dark:bg-gray-900/40"}`),y(ae,`${E().label??""} `),y(re,E().desc)}),g(u,se)},n=(u,b=Ce,E=Ce)=>{const F=ce(()=>N(b().split(".").pop()??"")),L=ce(()=>a(O).has(b()));var V=Re(),se=xe(V);{var le=X=>{var re=Gc(),ie=i(re);M(()=>{Qe(re,1,`relative inline-flex h-6 w-11 items-center rounded-full transition-colors ${E()?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Qe(ie,1,`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${E()?"translate-x-6":"translate-x-1"}`)}),te("click",re,()=>J(b(),!E())),g(X,re)},ve=X=>{var re=Kc();M(()=>hr(re,E())),te("input",re,ie=>{const de=parseFloat(ie.target.value);isNaN(de)||J(b(),de)}),g(X,re)},Ie=X=>{var re=Yc(),ie=i(re);Xe(ie,17,E,rt,(pe,Q,he)=>{var Ne=Jc(),vt=i(Ne),xt=p(vt,2);M(()=>hr(vt,a(Q))),te("input",vt,Le=>{const ye=[...je(a(c),b())||[]];ye[he]=Le.target.value,J(b(),ye)}),te("click",xt,()=>{const Le=(je(a(c),b())||[]).filter((ye,Ke)=>Ke!==he);J(b(),Le)}),g(pe,Ne)});var de=p(ie,2);te("click",de,()=>{const pe=[...je(a(c),b())||[],""];J(b(),pe)}),g(X,re)},ae=ce(()=>Array.isArray(E())),Y=X=>{var re=Xc(),ie=i(re),de=p(ie,2),pe=i(de);M(()=>{$e(ie,"type",a(L)?"text":"password"),hr(ie,E()??""),y(pe,a(L)?"隐藏":"显示")}),te("input",ie,Q=>J(b(),Q.target.value)),te("click",de,()=>H(b())),g(X,re)},fe=X=>{var re=Qc();M(()=>hr(re,E()??"")),te("input",re,ie=>J(b(),ie.target.value)),g(X,re)};z(se,X=>{typeof E()=="boolean"?X(le):typeof E()=="number"?X(ve,1):a(ae)?X(Ie,2):a(F)?X(Y,3):X(fe,-1)})}g(u,V)},o=(u,b=Ce,E=Ce)=>{const F=ce(()=>JSON.stringify(E(),null,2)),L=ce(()=>Math.min(15,(a(F).match(/\n/g)||[]).length+2));var V=Zc();M(()=>{hr(V,a(F)),$e(V,"rows",a(L))}),xr("blur",V,se=>{try{const le=JSON.parse(se.target.value);J(b(),le)}catch{se.target.value=JSON.stringify(je(a(c),b())??E(),null,2)}}),g(u,V)},s=(u,b=Ce,E=Ce,F=Ce)=>{const L=ce(()=>je(a(c),b())??F()),V=ce(()=>a(He).has(b()));var se=nu(),le=i(se);{var ve=Y=>{var fe=tu(),X=xe(fe),re=i(X);Cd(re,{size:13,class:"flex-shrink-0 text-gray-400"});var ie=p(re,2),de=i(ie),pe=p(ie,2);{var Q=Ne=>{var vt=eu();g(Ne,vt)};z(pe,Ne=>{a(V)&&Ne(Q)})}var he=p(X,2);o(he,b,()=>a(L)),M(()=>y(de,E())),g(Y,fe)},Ie=ce(()=>me(a(L))),ae=Y=>{var fe=au(),X=i(fe),re=i(X),ie=p(re);{var de=he=>{var Ne=ru();g(he,Ne)};z(ie,he=>{a(V)&&he(de)})}var pe=p(X,2),Q=i(pe);n(Q,b,()=>a(L)),M(()=>y(re,`${E()??""} `)),g(Y,fe)};z(le,Y=>{a(Ie)?Y(ve):Y(ae,-1)})}M(()=>Qe(se,1,`rounded-lg border p-3 transition-colors ${a(V)?"border-sky-500/50 bg-sky-500/5":"border-gray-200 bg-gray-50/40 dark:border-gray-700 dark:bg-gray-900/40"}`)),g(u,se)},l=(u,b=Ce,E=Ce,F=Ce)=>{const L=ce(()=>S(F())),V=ce(()=>yt(b()));var se=iu(),le=i(se),ve=i(le),Ie=i(ve),ae=p(ve,2);{var Y=Q=>{var he=ou();g(Q,he)};z(ae,Q=>{a(V)&&Q(Y)})}var fe=p(ae,2);{var X=Q=>{var he=su(),Ne=i(he);M(vt=>y(Ne,vt),[()=>m(F())]),g(Q,he)};z(fe,Q=>{a(L)||Q(X)})}var re=p(le,2),ie=i(re);{var de=Q=>{var he=Re(),Ne=xe(he);Xe(Ne,17,()=>Object.entries(F()),rt,(vt,xt)=>{var Le=ce(()=>Na(a(xt),2));let ye=()=>a(Le)[0],Ke=()=>a(Le)[1];const dt=ce(()=>`${b()}.${ye()}`);var It=Re(),Ve=xe(It);{var At=Rt=>{s(Rt,()=>a(dt),ye,Ke)},Ht=ce(()=>S(Ke())),Er=Rt=>{s(Rt,()=>a(dt),ye,Ke)};z(Ve,Rt=>{a(Ht)?Rt(At):Rt(Er,-1)})}g(vt,It)}),g(Q,he)},pe=Q=>{s(Q,b,E,F)};z(ie,Q=>{a(L)?Q(de):Q(pe,-1)})}M(()=>y(Ie,E())),g(u,se)},d=(u,b=Ce,E=Ce)=>{var F=Re(),L=xe(F);{var V=ae=>{var Y=lu();Xe(Y,21,()=>Object.entries(E()),rt,(fe,X)=>{var re=ce(()=>Na(a(X),2));let ie=()=>a(re)[0],de=()=>a(re)[1];var pe=Re(),Q=xe(pe);{var he=xt=>{l(xt,()=>`${b()}.${ie()}`,ie,de)},Ne=ce(()=>S(de())),vt=xt=>{s(xt,()=>`${b()}.${ie()}`,ie,de)};z(Q,xt=>{a(Ne)?xt(he):xt(vt,-1)})}g(fe,pe)}),g(ae,Y)},se=ce(()=>S(E())),le=ae=>{s(ae,b,b,E)},ve=ce(()=>Array.isArray(E())),Ie=ae=>{s(ae,b,b,E)};z(L,ae=>{a(se)?ae(V):a(ve)?ae(le,1):ae(Ie,-1)})}g(u,F)};let c=R(null),f=R(null),_=R(null),w=R(!0),k=R(!1),I=R(""),T=R(""),j=R(!1),C=R(!1),O=R(mt(new Set)),q=R("provider");const K={provider:zd,gateway:Pd,channels:Id,agent:xd,memory:kd,security:Dd,heartbeat:Od,reliability:Ps,scheduler:$d,sessions_spawn:Td,observability:Sd,web_search:jd,cost:Nd,runtime:Hd,tunnel:wd,identity:_d};function S(u){return u!==null&&typeof u=="object"&&!Array.isArray(u)}function m(u){return typeof u=="boolean"?"bool":typeof u=="number"?"number":Array.isArray(u)?"array":S(u)?"object":"string"}function N(u){const b=String(u).toLowerCase();return["key","token","secret","password","auth","credential","private"].some(E=>b.includes(E))}function P(u){if(!a(c))return[];const b=new Set(Object.keys(u.fields)),E=new Set;for(const L of Object.keys(u.fields))E.add(L.split(".")[0]);const F=[];for(const L of E){const V=a(c)[L];if(S(V))for(const[se,le]of Object.entries(V)){const ve=`${L}.${se}`;b.has(ve)||F.push({path:ve,key:se,value:le})}}return F}const W=ce(()=>a(c)?Object.keys(a(c)).filter(u=>!Cs.has(u)).sort():[]),Se=Object.entries(Qa),_e=ce(()=>Dn(a(c)));function je(u,b){if(!u)return;const E=b.split(".");let F=u;for(const L of E){if(F==null||typeof F!="object")return;F=F[L]}return F}function qe(u,b,E){const F=b.split(".");let L=u;for(let V=0;V<F.length-1;V++)(L[F[V]]==null||typeof L[F[V]]!="object")&&(L[F[V]]={}),L=L[F[V]];L[F[F.length-1]]=E}function B(u){if(a(c))return je(a(c),u)}function Z(u){return JSON.parse(JSON.stringify(u))}function ee(u,b){return JSON.stringify(u)===JSON.stringify(b)}function oe(u,b,E){const F=[],L=new Set([...Object.keys(u||{}),...Object.keys(b||{})]);for(const V of L){const se=E?`${E}.${V}`:V,le=(u||{})[V],ve=(b||{})[V];S(le)&&S(ve)?F.push(...oe(le,ve,se)):ee(le,ve)||F.push({fieldPath:se,newVal:le,oldVal:ve})}return F}function Oe(){return!a(c)||!a(f)?[]:oe(a(c),a(f),"").map(b=>{for(const F of Object.values(Qa))if(F.fields[b.fieldPath])return{...b,label:F.fields[b.fieldPath].label,group:F.label};const E=b.fieldPath.split(".");return{...b,label:Hn(E[E.length-1]),group:Hn(E[0])}})}const ze=ce(()=>!!(a(c)&&a(f)&&JSON.stringify(a(c))!==JSON.stringify(a(f)))),ke=ce(Oe),He=ce(()=>new Set(a(ke).map(u=>u.fieldPath)));function yt(u){for(const b of a(He))if(b===u||b.startsWith(u+"."))return!0;return!1}function be(u){v(q,u,!0),Ms(u)}function ne(){if(typeof window>"u")return;const u=window.location.hash.replace(/^#/,"");if(!u.startsWith("config-section-"))return;const b=u.replace(/^config-section-/,"");a(_e).some(E=>E.groupKey===b)&&be(b)}const we=ce(()=>a(c)?JSON.stringify(a(c),null,2):""),D=ce(()=>Lc(a(we)));function J(u,b){if(!a(c))return;const E=Z(a(c));qe(E,u,b),v(c,E,!0)}function Ye(u){const b=B(u),E=Array.isArray(b)?[...b,""]:[""];J(u,E)}function ot(u,b){const E=B(u);Array.isArray(E)&&J(u,E.filter((F,L)=>L!==b))}function _t(u,b,E){const F=B(u);if(!Array.isArray(F))return;const L=[...F];L[b]=E,J(u,L)}function H(u){const b=new Set(a(O));b.has(u)?b.delete(u):b.add(u),v(O,b,!0)}function U(u){return u==null?"null":typeof u=="boolean"?u?"true":"false":Array.isArray(u)||typeof u=="object"?JSON.stringify(u):String(u)}function me(u){return!!(S(u)||Array.isArray(u)&&u.some(b=>S(b)||Array.isArray(b)))}async function st(){try{const[u,b]=await Promise.all([ht.getConfig(),ht.getStatus().catch(()=>null)]);v(c,typeof u=="object"&&u?u:{},!0),v(f,Z(a(c)),!0),v(_,b,!0),v(I,"")}catch(u){v(I,u instanceof Error?u.message:"Failed to load config",!0)}finally{v(w,!1)}}async function lt(){if(!(!a(ze)||a(k))){v(k,!0),v(T,"");try{const u={};for(const E of a(ke))qe(u,E.fieldPath,E.newVal);const b=await ht.saveConfig(u);v(f,Z(a(c)),!0),v(C,!1),b!=null&&b.restart_required?v(T,"已保存，部分设置需要重启服务后生效"):v(T,"已保存"),setTimeout(()=>{v(T,"")},5e3)}catch(u){v(T,"保存失败: "+(u instanceof Error?u.message:String(u)))}finally{v(k,!1)}}}function et(){a(f)&&(v(c,Z(a(f)),!0),v(C,!1))}async function tt(){if(!(!a(we)||typeof navigator>"u"||!navigator.clipboard))try{await navigator.clipboard.writeText(a(we))}catch{}}Lt(()=>{st()}),Lt(()=>{a(w)||a(j)||a(_e).length===0||queueMicrotask(()=>{ne()})});var De=Au(),Ge=i(De),at=i(Ge),Be=i(at),St=p(at,2),ge=i(St),ue=i(ge),Ee=p(ge,2),Ze=p(Ge,2);{var Ae=u=>{var b=du();g(u,b)},it=u=>{var b=cu(),E=i(b);M(()=>y(E,a(I))),g(u,b)},ft=u=>{var b=uu(),E=i(b),F=i(E),L=i(F);vl(L,()=>a(D)),g(u,b)},nt=u=>{var b=xu(),E=i(b),F=i(E);Xe(F,21,()=>a(_e),rt,(le,ve)=>{const Ie=ce(()=>yt(a(ve).groupKey));var ae=gu(),Y=i(ae),fe=i(Y),X=p(Y,2);{var re=pe=>{var Q=fu();g(pe,Q)};z(X,pe=>{a(ve).dynamic&&pe(re)})}var ie=p(X,2);{var de=pe=>{var Q=vu();g(pe,Q)};z(ie,pe=>{a(Ie)&&pe(de)})}M(()=>{Qe(ae,1,`inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-sm transition ${a(q)===a(ve).groupKey?"border-sky-500 bg-sky-500/10 text-sky-700 dark:text-sky-300":"border-gray-300 bg-white text-gray-600 hover:border-sky-400 hover:text-sky-600 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:border-sky-500 dark:hover:text-sky-300"}`),y(fe,a(ve).label)}),te("click",ae,()=>be(a(ve).groupKey)),g(le,ae)});var L=p(E,2);Xe(L,17,()=>Se,rt,(le,ve)=>{var Ie=ce(()=>Na(a(ve),2));let ae=()=>a(Ie)[0],Y=()=>a(Ie)[1];const fe=ce(()=>K[ae()]),X=ce(()=>P(Y())),re=ce(()=>Object.keys(Y().fields)),ie=ce(()=>a(re).some(Ve=>a(He).has(Ve))||a(X).some(Ve=>yt(Ve.path)));var de=bu(),pe=i(de),Q=i(pe);{var he=Ve=>{var At=Re(),Ht=xe(At);gl(Ht,()=>a(fe),(Er,Rt)=>{Rt(Er,{size:18,class:"text-gray-500 dark:text-gray-400"})}),g(Ve,At)};z(Q,Ve=>{a(fe)&&Ve(he)})}var Ne=p(Q,2),vt=i(Ne),xt=p(Ne,2);{var Le=Ve=>{var At=pu();g(Ve,At)};z(xt,Ve=>{a(ie)&&Ve(Le)})}var ye=p(pe,2),Ke=i(ye);Xe(Ke,17,()=>Object.entries(Y().fields),rt,(Ve,At)=>{var Ht=ce(()=>Na(a(At),2));r(Ve,()=>a(Ht)[0],()=>a(Ht)[1])});var dt=p(Ke,2);{var It=Ve=>{var At=yu(),Ht=p(i(At),2);Xe(Ht,21,()=>a(X),rt,(Er,Rt)=>{let Ba=()=>a(Rt).path,Ea=()=>a(Rt).key,ea=()=>a(Rt).value;var ta=Re(),fn=xe(ta);{var vn=ra=>{l(ra,Ba,Ea,ea)},Os=ce(()=>S(ea())),Fs=ra=>{s(ra,Ba,Ea,ea)};z(fn,ra=>{a(Os)?ra(vn):ra(Fs,-1)})}g(Er,ta)}),g(Ve,At)};z(dt,Ve=>{a(X).length>0&&Ve(It)})}M(Ve=>{$e(de,"id",Ve),de.open=Y().defaultOpen,y(vt,Y().label)},[()=>Za(ae())]),xr("toggle",de,Ve=>{Ve.currentTarget.open&&v(q,ae(),!0)}),g(le,de)});var V=p(L,2);{var se=le=>{var ve=_u(),Ie=p(i(ve),2);Xe(Ie,21,()=>a(W),rt,(ae,Y)=>{const fe=ce(()=>a(c)[a(Y)]),X=ce(()=>yt(a(Y))),re=ce(()=>m(a(fe)));var ie=mu(),de=i(ie),pe=i(de);Md(pe,{size:18,class:"flex-shrink-0 text-gray-400 dark:text-gray-500"});var Q=p(pe,2),he=i(Q),Ne=p(Q,2);{var vt=dt=>{var It=hu();g(dt,It)};z(Ne,dt=>{a(X)&&dt(vt)})}var xt=p(Ne,2),Le=i(xt),ye=p(de,2),Ke=i(ye);d(Ke,()=>a(Y),()=>a(fe)),M(dt=>{$e(ie,"id",dt),y(he,a(Y)),y(Le,a(re))},[()=>Za(a(Y))]),xr("toggle",ie,dt=>{dt.currentTarget.open&&v(q,a(Y),!0)}),g(ae,ie)}),g(le,ve)};z(V,le=>{a(W).length>0&&le(se)})}g(u,b)};z(Ze,u=>{a(w)?u(Ae):a(I)?u(it,1):a(j)?u(ft,2):u(nt,-1)})}var bt=p(Ze,2);{var Et=u=>{var b=Su(),E=i(b),F=i(E),L=i(F),V=i(L),se=p(L,2),le=i(se),ve=p(F,2),Ie=i(ve),ae=p(Ie,2),Y=i(ae),fe=p(E,2);{var X=re=>{var ie=wu(),de=p(i(ie),2);Xe(de,21,()=>a(ke),rt,(pe,Q)=>{var he=ku(),Ne=i(he),vt=i(Ne),xt=p(Ne,2),Le=i(xt),ye=p(xt,2),Ke=i(ye),dt=p(ye,4),It=i(dt);M((Ve,At)=>{y(vt,a(Q).group),y(Le,a(Q).label),y(Ke,Ve),y(It,At)},[()=>U(a(Q).oldVal),()=>U(a(Q).newVal)]),g(pe,he)}),g(re,ie)};z(fe,re=>{a(C)&&re(X)})}M(()=>{y(V,`${a(ke).length??""} 项更改`),y(le,a(C)?"隐藏详情":"查看详情"),ae.disabled=a(k),y(Y,a(k)?"保存中...":"保存配置")}),te("click",se,()=>v(C,!a(C))),te("click",Ie,et),te("click",ae,lt),g(u,b)};z(bt,u=>{a(ze)&&!a(w)&&!a(j)&&u(Et)})}var A=p(bt,2);{var G=u=>{var b=Eu(),E=i(b);M(F=>{Qe(b,1,`fixed bottom-20 left-1/2 z-50 -translate-x-1/2 rounded-lg border px-4 py-2 text-sm shadow-lg ${F??""}`),y(E,a(T))},[()=>a(T).startsWith("保存失败")?"border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300":"border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300"]),g(u,b)};z(A,u=>{a(T)&&u(G)})}M(u=>{y(Be,u),y(ue,a(j)?"结构化编辑":"JSON 视图")},[()=>h("config.title")]),te("click",ge,()=>v(j,!a(j))),te("click",Ee,tt),g(e,De),Pe()}ir(["click","change","input"]);var Cu=x('<p class="text-gray-400 dark:text-gray-500"> </p>'),Mu=x('<li class="whitespace-pre-wrap break-words"><span class="mr-3 select-none text-gray-400 dark:text-gray-600"> </span> <span> </span></li>'),Nu=x('<ol class="space-y-1"></ol>'),Tu=x('<section class="space-y-4"><div class="flex flex-wrap items-center justify-between gap-3"><h2 class="text-2xl font-semibold"> </h2> <div class="flex items-center gap-2"><span> </span> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></div> <div class="h-[65vh] overflow-y-auto rounded-xl border border-gray-200 bg-gray-50 p-4 font-mono text-xs leading-5 text-green-800 dark:border-gray-700 dark:bg-gray-950 dark:text-green-300"><!></div></section>');function Pu(e,t){Te(t,!0);const r=1e3,n=500,o=1e4;let s=R(mt([])),l=R(!1),d=R("disconnected"),c=R(null),f=null,_=null,w=0,k=!0;const I=ce(()=>a(d)==="connected"?"border-green-500/50 bg-green-500/15 text-green-700 dark:text-green-300":a(d)==="reconnecting"?"border-amber-500/50 bg-amber-500/15 text-amber-700 dark:text-amber-200":"border-red-500/50 bg-red-500/15 text-red-700 dark:text-red-300"),T=ce(()=>a(d)==="connected"?h("logs.connected"):a(d)==="reconnecting"?h("logs.reconnecting"):h("logs.disconnected"));function j(be){const ne=Xa?new URL(Xa,window.location.href):new URL(window.location.href);return ne.protocol=ne.protocol==="https:"?"wss:":"ws:",ne.pathname="/api/logs/stream",ne.search=`token=${encodeURIComponent(be)}`,ne.hash="",ne.toString()}function C(be){if(typeof be!="string"||be.length===0)return;const ne=be.split(/\r?\n/).filter(D=>D.length>0);if(ne.length===0)return;const we=[...a(s),...ne];v(s,we.length>r?we.slice(we.length-r):we,!0)}function O(){_!==null&&(clearTimeout(_),_=null)}function q(){f&&(f.onopen=null,f.onmessage=null,f.onerror=null,f.onclose=null,f.close(),f=null)}function K(){if(!k){v(d,"disconnected");return}v(d,"reconnecting");const be=Math.min(n*2**w,o);w+=1,O(),_=setTimeout(()=>{_=null,S()},be)}function S(){O();const be=Ra();if(!be){v(d,"disconnected");return}v(d,"reconnecting"),q();let ne;try{ne=new WebSocket(j(be))}catch{K();return}f=ne,ne.onopen=()=>{w=0,v(d,"connected")},ne.onmessage=we=>{a(l)||C(we.data)},ne.onerror=()=>{(ne.readyState===WebSocket.OPEN||ne.readyState===WebSocket.CONNECTING)&&ne.close()},ne.onclose=()=>{f=null,K()}}function m(){v(l,!a(l))}function N(){v(s,[],!0)}Lt(()=>(k=!0,S(),()=>{k=!1,O(),q(),v(d,"disconnected")})),Lt(()=>{a(s).length,a(l),!(a(l)||!a(c))&&queueMicrotask(()=>{a(c)&&(a(c).scrollTop=a(c).scrollHeight)})});var P=Tu(),W=i(P),Se=i(W),_e=i(Se),je=p(Se,2),qe=i(je),B=i(qe),Z=p(qe,2),ee=i(Z),oe=p(Z,2),Oe=i(oe),ze=p(W,2),ke=i(ze);{var He=be=>{var ne=Cu(),we=i(ne);M(D=>y(we,D),[()=>h("logs.waiting")]),g(be,ne)},yt=be=>{var ne=Nu();Xe(ne,21,()=>a(s),rt,(we,D,J)=>{var Ye=Mu(),ot=i(Ye),_t=i(ot),H=p(ot,2),U=i(H);M(me=>{y(_t,me),y(U,a(D))},[()=>String(J+1).padStart(4,"0")]),g(we,Ye)}),g(be,ne)};z(ke,be=>{a(s).length===0?be(He):be(yt,-1)})}jn(ze,be=>v(c,be),()=>a(c)),M((be,ne,we)=>{y(_e,be),Qe(qe,1,`rounded-full border px-2 py-1 text-xs font-medium uppercase tracking-wide ${a(I)}`),y(B,a(T)),y(ee,ne),y(Oe,we)},[()=>h("logs.title"),()=>a(l)?h("logs.resume"):h("logs.pause"),()=>h("logs.clear")]),te("click",Z,m),te("click",oe,N),g(e,P),Pe()}ir(["click"]);var Ou=x("<option> </option>"),Fu=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Iu=x('<div class="space-y-3 rounded-xl border border-sky-500/30 bg-white p-4 dark:bg-gray-800"><h3 class="text-base font-semibold text-gray-900 dark:text-gray-100"> </h3> <div class="grid gap-3 sm:grid-cols-2"><div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select></div> <div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="number" min="1000" step="1000" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="sm:col-span-2"><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="flex items-center gap-2"><span class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </span> <button type="button" disabled=""><span></span></button> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></div></div> <!> <div class="flex justify-end gap-2 pt-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"> </button></div></div>'),Lu=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Ru=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),ju=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Hu=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Du=x("<option> </option>"),Uu=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),zu=x('<div class="space-y-3"><div class="grid gap-3 sm:grid-cols-2"><div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select></div> <div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="number" min="1000" step="1000" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="sm:col-span-2"><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="flex items-center gap-2"><span class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </span> <button type="button" disabled=""><span></span></button> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></div></div> <!> <div class="flex justify-end gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"> </button></div></div>'),Bu=x('<div class="flex items-start justify-between gap-3"><div class="min-w-0 flex-1"><div class="flex items-center gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-2 font-mono text-sm text-gray-500 dark:text-gray-400"> </p> <p class="mt-1 text-xs text-gray-400 dark:text-gray-500"> </p></div> <div class="flex items-center gap-2"><button type="button"><span></span></button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-2 py-1 text-xs text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-red-500/50 bg-red-500/10 px-2 py-1 text-xs text-red-600 hover:bg-red-500/20 disabled:opacity-50 dark:text-red-300"> </button></div></div>'),Wu=x('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><!></article>'),qu=x('<!> <div class="space-y-3"></div>',1),Vu=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div> <!> <!></section>');function Gu(e,t){Te(t,!0);const r=["agent_start","agent_end","llm_request","llm_response","tool_call_start","tool_call_end","message_received","message_sent"];let n=R(mt([])),o=R(!0),s=R(""),l=R(""),d=R(null),c=R(!1),f=R(!1),_=R(""),w=R(""),k="hook-add",I=R(mt(r[0])),T=R(""),j=R(3e4),C=R(!0);function O(){v(I,r[0],!0),v(T,""),v(j,3e4),v(C,!0)}function q(D,J){return`${D}-${J}`}function K(D){return D.split("_").map(J=>J.charAt(0).toUpperCase()+J.slice(1)).join(" ")}function S(){return a(T).trim()?!Number.isFinite(Number(a(j)))||Number(a(j))<1e3?(v(l,h("hooks.timeoutInvalid"),!0),!1):!0:(v(l,h("hooks.commandRequired"),!0),!1)}async function m(){v(o,!0);try{const D=await ht.getHooks();v(n,Array.isArray(D==null?void 0:D.hooks)?D.hooks:[],!0),v(s,""),v(l,"")}catch(D){v(n,[],!0),v(s,D instanceof Error?D.message:h("hooks.loadFailed"),!0)}finally{v(o,!1)}}function N(D){v(d,D.id,!0),v(l,""),v(I,D.event,!0),v(T,D.command,!0),v(j,D.timeout_ms,!0),v(C,D.enabled,!0)}function P(){v(d,null),v(l,""),O()}async function W(D){if(S()){v(f,!0),v(l,"");try{await ht.updateHook(D,{event:a(I),command:a(T).trim(),timeout_ms:Number(a(j))}),v(d,null),O(),await m()}catch(J){v(l,J instanceof Error?J.message:h("hooks.saveFailed"),!0)}finally{v(f,!1)}}}async function Se(){if(S()){v(f,!0),v(l,"");try{await ht.createHook({event:a(I),command:a(T).trim(),timeout_ms:Number(a(j))}),v(c,!1),O(),await m()}catch(D){v(l,D instanceof Error?D.message:h("hooks.saveFailed"),!0)}finally{v(f,!1)}}}async function _e(D){v(_,D,!0),v(l,"");try{await ht.deleteHook(D),a(d)===D&&P(),await m()}catch(J){v(l,J instanceof Error?J.message:h("hooks.deleteFailed"),!0)}finally{v(_,"")}}async function je(D){v(w,D,!0),v(l,"");try{await ht.toggleHook(D),await m()}catch(J){v(l,J instanceof Error?J.message:h("hooks.toggleFailed"),!0)}finally{v(w,"")}}Lt(()=>{m()});var qe=Vu(),B=i(qe),Z=i(B),ee=i(Z),oe=p(Z,2),Oe=i(oe),ze=p(B,2);{var ke=D=>{var J=Iu(),Ye=i(J),ot=i(Ye),_t=p(Ye,2),H=i(_t),U=i(H),me=i(U),st=p(U,2);Xe(st,21,()=>r,rt,(E,F)=>{var L=Ou(),V=i(L),se={};M(le=>{y(V,le),se!==(se=a(F))&&(L.value=(L.__value=a(F))??"")},[()=>K(a(F))]),g(E,L)});var lt=p(H,2),et=i(lt),tt=i(et),De=p(et,2),Ge=p(lt,2),at=i(Ge),Be=i(at),St=p(at,2),ge=p(Ge,2),ue=i(ge),Ee=i(ue),Ze=p(ue,2),Ae=i(Ze),it=p(Ze,2),ft=i(it),nt=p(_t,2);{var bt=E=>{var F=Fu(),L=i(F);M(()=>y(L,a(l))),g(E,F)};z(nt,E=>{a(l)&&E(bt)})}var Et=p(nt,2),A=i(Et),G=i(A),u=p(A,2),b=i(u);M((E,F,L,V,se,le,ve,Ie,ae,Y,fe,X,re,ie,de,pe)=>{y(ot,E),$e(U,"for",F),y(me,L),$e(st,"id",V),$e(et,"for",se),y(tt,le),$e(De,"id",ve),$e(at,"for",Ie),y(Be,ae),$e(St,"id",Y),$e(St,"placeholder",fe),y(Ee,X),$e(Ze,"aria-label",re),Qe(Ze,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a(C)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Qe(Ae,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(C)?"translate-x-4":"translate-x-1"}`),y(ft,ie),y(G,de),u.disabled=a(f),y(b,pe)},[()=>h("hooks.newHook"),()=>q(k,"event"),()=>h("hooks.event"),()=>q(k,"event"),()=>q(k,"timeout"),()=>h("hooks.timeout"),()=>q(k,"timeout"),()=>q(k,"command"),()=>h("hooks.command"),()=>q(k,"command"),()=>h("hooks.commandPlaceholder"),()=>h("hooks.enabled"),()=>h("hooks.enabled"),()=>h("hooks.globalToggleHint"),()=>h("hooks.cancel"),()=>a(f)?h("hooks.saving"):h("hooks.save")]),Rn(st,()=>a(I),E=>v(I,E)),Br(De,()=>a(j),E=>v(j,E)),Br(St,()=>a(T),E=>v(T,E)),te("click",A,()=>{v(c,!1),v(l,""),O()}),te("click",u,Se),g(D,J)};z(ze,D=>{a(c)&&D(ke)})}var He=p(ze,2);{var yt=D=>{var J=Lu(),Ye=i(J);M(ot=>y(Ye,ot),[()=>h("hooks.loading")]),g(D,J)},be=D=>{var J=Ru(),Ye=i(J);M(()=>y(Ye,a(s))),g(D,J)},ne=D=>{var J=ju(),Ye=i(J);M(ot=>y(Ye,ot),[()=>h("hooks.noHooks")]),g(D,J)},we=D=>{var J=qu(),Ye=xe(J);{var ot=H=>{var U=Hu(),me=i(U);M(()=>y(me,a(l))),g(H,U)};z(Ye,H=>{a(l)&&H(ot)})}var _t=p(Ye,2);Xe(_t,21,()=>a(n),H=>H.id,(H,U)=>{var me=Wu(),st=i(me);{var lt=tt=>{var De=zu(),Ge=i(De),at=i(Ge),Be=i(at),St=i(Be),ge=p(Be,2);Xe(ge,21,()=>r,rt,(Y,fe)=>{var X=Du(),re=i(X),ie={};M(de=>{y(re,de),ie!==(ie=a(fe))&&(X.value=(X.__value=a(fe))??"")},[()=>K(a(fe))]),g(Y,X)});var ue=p(at,2),Ee=i(ue),Ze=i(Ee),Ae=p(Ee,2),it=p(ue,2),ft=i(it),nt=i(ft),bt=p(ft,2),Et=p(it,2),A=i(Et),G=i(A),u=p(A,2),b=i(u),E=p(u,2),F=i(E),L=p(Ge,2);{var V=Y=>{var fe=Uu(),X=i(fe);M(()=>y(X,a(l))),g(Y,fe)};z(L,Y=>{a(l)&&Y(V)})}var se=p(L,2),le=i(se),ve=i(le),Ie=p(le,2),ae=i(Ie);M((Y,fe,X,re,ie,de,pe,Q,he,Ne,vt,xt,Le,ye)=>{$e(Be,"for",Y),y(St,fe),$e(ge,"id",X),$e(Ee,"for",re),y(Ze,ie),$e(Ae,"id",de),$e(ft,"for",pe),y(nt,Q),$e(bt,"id",he),y(G,Ne),$e(u,"aria-label",vt),Qe(u,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a(C)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Qe(b,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(C)?"translate-x-4":"translate-x-1"}`),y(F,xt),y(ve,Le),Ie.disabled=a(f),y(ae,ye)},[()=>q(a(U).id,"event"),()=>h("hooks.event"),()=>q(a(U).id,"event"),()=>q(a(U).id,"timeout"),()=>h("hooks.timeout"),()=>q(a(U).id,"timeout"),()=>q(a(U).id,"command"),()=>h("hooks.command"),()=>q(a(U).id,"command"),()=>h("hooks.enabled"),()=>h("hooks.enabled"),()=>h("hooks.globalToggleHint"),()=>h("hooks.cancel"),()=>a(f)?h("hooks.saving"):h("hooks.save")]),Rn(ge,()=>a(I),Y=>v(I,Y)),Br(Ae,()=>a(j),Y=>v(j,Y)),Br(bt,()=>a(T),Y=>v(T,Y)),te("click",le,P),te("click",Ie,()=>W(a(U).id)),g(tt,De)},et=tt=>{var De=Bu(),Ge=i(De),at=i(Ge),Be=i(at),St=i(Be),ge=p(Be,2),ue=i(ge),Ee=p(at,2),Ze=i(Ee),Ae=p(Ee,2),it=i(Ae),ft=p(Ge,2),nt=i(ft),bt=i(nt),Et=p(nt,2),A=i(Et),G=p(Et,2),u=i(G);M((b,E,F,L,V,se)=>{y(St,b),Qe(ge,1,`rounded-full px-2 py-1 text-xs font-medium ${a(U).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),y(ue,E),y(Ze,a(U).command),y(it,`${F??""}: ${a(U).timeout_ms??""}ms`),nt.disabled=a(w)===a(U).id,$e(nt,"aria-label",L),Qe(nt,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a(U).enabled?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Qe(bt,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(U).enabled?"translate-x-4":"translate-x-1"}`),y(A,V),G.disabled=a(_)===a(U).id,y(u,se)},[()=>K(a(U).event),()=>a(U).enabled?h("common.enabled"):h("common.disabled"),()=>h("hooks.timeout"),()=>a(U).enabled?h("common.disabled"):h("common.enabled"),()=>h("hooks.edit"),()=>a(_)===a(U).id?h("hooks.deleting"):h("hooks.delete")]),te("click",nt,()=>je(a(U).id)),te("click",Et,()=>N(a(U))),te("click",G,()=>_e(a(U).id)),g(tt,De)};z(st,tt=>{a(d)===a(U).id?tt(lt):tt(et,-1)})}g(H,me)}),g(D,J)};z(He,D=>{a(o)?D(yt):a(s)?D(be,1):a(n).length===0?D(ne,2):D(we,-1)})}M((D,J)=>{y(ee,D),y(Oe,J)},[()=>h("hooks.title"),()=>a(c)?h("hooks.cancelAdd"):h("hooks.addHook")]),te("click",oe,()=>{v(c,!a(c)),v(l,""),a(c)&&O()}),g(e,qe),Pe()}ir(["click"]);var Ku=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Ju=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Yu=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Xu=x('<p class="mt-1 text-xs text-gray-500 dark:text-gray-400"> </p>'),Qu=x('<div class="rounded-lg border border-gray-200 bg-gray-50/60 p-3 dark:border-gray-700 dark:bg-gray-900/60"><p class="font-mono text-sm font-medium text-gray-700 dark:text-gray-200"> </p> <!></div>'),Zu=x('<div class="border-t border-gray-200 p-4 dark:border-gray-700"><h4 class="mb-3 text-sm font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </h4> <div class="grid gap-2"></div></div>'),e0=x('<div class="border-t border-gray-200 p-4 dark:border-gray-700"><p class="text-sm text-gray-500 dark:text-gray-400"> </p></div>'),t0=x('<article class="rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><button type="button" class="flex w-full items-center justify-between gap-3 p-4 text-left"><div class="min-w-0 flex-1"><div class="flex items-center gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-1 font-mono text-sm text-gray-500 dark:text-gray-400"> </p></div> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></button> <!></article>'),r0=x('<div class="space-y-4"></div>'),a0=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!></section>');function n0(e,t){Te(t,!0);let r=R(mt([])),n=R(!0),o=R(""),s=R(null);async function l(){v(n,!0);try{const S=await ht.getMcpServers();v(r,Array.isArray(S==null?void 0:S.servers)?S.servers:[],!0),v(o,"")}catch(S){v(r,[],!0),v(o,S instanceof Error?S.message:h("mcp.loadFailed"),!0)}finally{v(n,!1)}}function d(S){v(s,a(s)===S?null:S,!0)}async function c(){await l()}Lt(()=>{l()});var f=a0(),_=i(f),w=i(_),k=i(w),I=p(w,2),T=i(I),j=p(_,2);{var C=S=>{var m=Ku(),N=i(m);M(P=>y(N,P),[()=>h("mcp.loading")]),g(S,m)},O=S=>{var m=Ju(),N=i(m);M(()=>y(N,a(o))),g(S,m)},q=S=>{var m=Yu(),N=i(m);M(P=>y(N,P),[()=>h("mcp.noServers")]),g(S,m)},K=S=>{var m=r0();Xe(m,21,()=>a(r),rt,(N,P)=>{var W=t0(),Se=i(W),_e=i(Se),je=i(_e),qe=i(je),B=i(qe),Z=p(qe,2),ee=i(Z),oe=p(je,2),Oe=i(oe),ze=p(_e,2),ke=i(ze),He=p(Se,2);{var yt=ne=>{var we=Zu(),D=i(we),J=i(D),Ye=p(D,2);Xe(Ye,21,()=>a(P).tools,rt,(ot,_t)=>{var H=Qu(),U=i(H),me=i(U),st=p(U,2);{var lt=et=>{var tt=Xu(),De=i(tt);M(()=>y(De,a(_t).description)),g(et,tt)};z(st,et=>{a(_t).description&&et(lt)})}M(()=>y(me,a(_t).name)),g(ot,H)}),M(ot=>y(J,ot),[()=>h("mcp.availableTools")]),g(ne,we)},be=ne=>{var we=e0(),D=i(we),J=i(D);M(Ye=>y(J,Ye),[()=>h("mcp.noTools")]),g(ne,we)};z(He,ne=>{a(s)===a(P).name&&a(P).tools&&a(P).tools.length>0?ne(yt):a(s)===a(P).name&&(!a(P).tools||a(P).tools.length===0)&&ne(be,1)})}M((ne,we)=>{var D;y(B,a(P).name),Qe(Z,1,`rounded-full px-2 py-1 text-xs font-medium ${a(P).status==="connected"?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":a(P).status==="connecting"?"border border-yellow-500/50 bg-yellow-500/20 text-yellow-700 dark:text-yellow-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),y(ee,ne),y(Oe,a(P).url),y(ke,`${((D=a(P).tools)==null?void 0:D.length)??0??""} ${we??""}`)},[()=>a(P).status==="connected"?h("mcp.connected"):a(P).status==="connecting"?h("mcp.connecting"):h("mcp.disconnected"),()=>h("mcp.tools")]),te("click",Se,()=>d(a(P).name)),g(N,W)}),g(S,m)};z(j,S=>{a(n)?S(C):a(o)?S(O,1):a(r).length===0?S(q,2):S(K,-1)})}M((S,m)=>{y(k,S),y(T,m)},[()=>h("mcp.title"),()=>h("common.refresh")]),te("click",I,c),g(e,f),Pe()}ir(["click"]);var o0=x('<span class="text-sm text-gray-500 dark:text-gray-400"> </span>'),s0=x("<div> </div>"),i0=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),l0=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),d0=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),c0=x('<p class="mt-2 text-sm text-gray-500 dark:text-gray-400"> </p>'),u0=x('<div class="flex items-center gap-2"><span class="text-xs text-yellow-600 dark:text-yellow-400"> </span> <button type="button" class="rounded px-2 py-1 text-xs font-medium text-red-500 transition hover:bg-red-500/20 disabled:opacity-50 dark:text-red-400"> </button> <button type="button" class="rounded px-2 py-1 text-xs text-gray-500 transition hover:bg-gray-200 dark:text-gray-400 dark:hover:bg-gray-700"> </button></div>'),f0=x('<button type="button" class="rounded px-2 py-1 text-xs text-red-500 transition hover:bg-red-500/20 dark:text-red-400"> </button>'),v0=x('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3></div> <!> <p class="mt-2 font-mono text-xs text-gray-400 dark:text-gray-500"> </p> <div class="mt-3 flex items-center justify-between gap-3"><div class="flex items-center gap-2"><span> </span> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></div> <!></div></article>'),g0=x('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),p0=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),y0=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),b0=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),h0=x('<p class="mt-2 line-clamp-3 text-sm text-gray-500 dark:text-gray-400"> </p>'),m0=x('<span class="flex items-center gap-1"><svg class="h-3.5 w-3.5" fill="currentColor" viewBox="0 0 20 20"><path d="M9.049 2.927c.3-.921 1.603-.921 1.902 0l1.07 3.292a1 1 0 00.95.69h3.462c.969 0 1.371 1.24.588 1.81l-2.8 2.034a1 1 0 00-.364 1.118l1.07 3.292c.3.921-.755 1.688-1.54 1.118l-2.8-2.034a1 1 0 00-1.175 0l-2.8 2.034c-.784.57-1.838-.197-1.539-1.118l1.07-3.292a1 1 0 00-.364-1.118L2.98 8.72c-.783-.57-.38-1.81.588-1.81h3.461a1 1 0 00.951-.69l1.07-3.292z"></path></svg> </span>'),_0=x('<span class="rounded bg-gray-100 px-1.5 py-0.5 dark:bg-gray-700"> </span>'),x0=x('<span class="rounded-full border border-green-500/50 bg-green-500/20 px-2 py-1 text-xs font-medium text-green-700 dark:text-green-300"> </span>'),k0=x('<button type="button" class="rounded-lg bg-sky-600 px-3 py-1 text-xs font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"> </button>'),w0=x('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-2"><div class="min-w-0 flex-1"><h3 class="truncate text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <p class="text-xs text-gray-400 dark:text-gray-500"> </p></div> <span class="rounded-full border border-gray-300 bg-gray-100 px-2 py-0.5 text-xs text-gray-600 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-300"> </span></div> <!> <div class="mt-3 flex flex-wrap items-center gap-2 text-xs text-gray-400 dark:text-gray-500"><!> <!> <span> </span></div> <div class="mt-3 flex items-center justify-between"><a target="_blank" rel="noopener noreferrer" class="text-xs text-sky-600 hover:underline dark:text-sky-400"> </a> <!></div></article>'),S0=x('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),E0=x('<div class="flex flex-col gap-3 sm:flex-row"><select class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200"><option>GitHub</option></select> <input type="text" class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 placeholder-gray-400 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:placeholder-gray-500"/> <button type="button" class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"> </button></div> <!>',1),A0=x('<section class="space-y-6"><div class="flex items-center justify-between"><div class="flex items-center gap-3"><h2 class="text-2xl font-semibold"> </h2> <!></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <div class="flex gap-1 rounded-lg border border-gray-200 bg-gray-100/50 p-1 dark:border-gray-700 dark:bg-gray-800/50"><button type="button"> </button> <button type="button"> </button></div> <!> <!> <!></section>');function $0(e,t){Te(t,!0);let r=R("installed"),n=R(mt([])),o=R(!0),s=R(""),l=R(""),d=R("success"),c=R(mt([])),f=R(!1),_=R(""),w=R(""),k=R("github"),I=R(!1),T=R(""),j=R(""),C=R("");function O(H,U="success"){v(l,H,!0),v(d,U,!0),setTimeout(()=>{v(l,"")},3e3)}async function q(){try{const H=await ht.getSkills();v(n,Array.isArray(H==null?void 0:H.skills)?H.skills:[],!0),v(s,"")}catch(H){v(n,[],!0),v(s,H instanceof Error?H.message:h("skills.loadFailed"),!0)}finally{v(o,!1)}}async function K(H){if(a(C)!==H){v(C,H,!0);return}v(C,""),v(j,H,!0);try{await ht.uninstallSkill(H),v(n,a(n).filter(U=>U.name!==H),!0),O(h("skills.uninstallSuccess"))}catch(U){O(h("skills.uninstallFailed")+(U!=null&&U.message?`: ${U.message}`:""),"error")}finally{v(j,"")}}const S=ce(()=>[...a(n)].sort((H,U)=>H.enabled===U.enabled?0:H.enabled?-1:1)),m=ce(()=>a(n).filter(H=>H.enabled).length);async function N(){!a(w).trim()&&a(k)==="github"&&v(w,"agent skill"),v(f,!0),v(I,!0),v(_,"");try{const H=await ht.discoverSkills(a(k),a(w));v(c,Array.isArray(H==null?void 0:H.results)?H.results:[],!0)}catch(H){v(c,[],!0),v(_,H instanceof Error?H.message:h("skills.searchFailed"),!0)}finally{v(f,!1)}}function P(H){return a(n).some(U=>U.name===H)}async function W(H,U){v(T,H,!0);try{const me=await ht.installSkill(H,U);me!=null&&me.skill&&v(n,[...a(n),{...me.skill,enabled:!0}],!0),O(h("skills.installSuccess"))}catch(me){O(h("skills.installFailed")+(me!=null&&me.message?`: ${me.message}`:""),"error")}finally{v(T,"")}}function Se(H){H.key==="Enter"&&N()}Lt(()=>{q()});var _e=A0(),je=i(_e),qe=i(je),B=i(qe),Z=i(B),ee=p(B,2);{var oe=H=>{var U=o0(),me=i(U);M(st=>y(me,`${a(m)??""}/${a(n).length??""} ${st??""}`),[()=>h("skills.active")]),g(H,U)};z(ee,H=>{!a(o)&&a(n).length>0&&H(oe)})}var Oe=p(qe,2),ze=i(Oe),ke=p(je,2),He=i(ke),yt=i(He),be=p(He,2),ne=i(be),we=p(ke,2);{var D=H=>{var U=s0(),me=i(U);M(()=>{Qe(U,1,`rounded-lg px-4 py-2 text-sm ${a(d)==="error"?"border border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300":"border border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300"}`),y(me,a(l))}),g(H,U)};z(we,H=>{a(l)&&H(D)})}var J=p(we,2);{var Ye=H=>{var U=Re(),me=xe(U);{var st=De=>{var Ge=i0(),at=i(Ge);M(Be=>y(at,Be),[()=>h("skills.loading")]),g(De,Ge)},lt=De=>{var Ge=l0(),at=i(Ge);M(()=>y(at,a(s))),g(De,Ge)},et=De=>{var Ge=d0(),at=i(Ge);M(Be=>y(at,Be),[()=>h("skills.noSkills")]),g(De,Ge)},tt=De=>{var Ge=g0();Xe(Ge,21,()=>a(S),rt,(at,Be)=>{var St=v0(),ge=i(St),ue=i(ge),Ee=i(ue),Ze=p(ge,2);{var Ae=L=>{var V=c0(),se=i(V);M(()=>y(se,a(Be).description)),g(L,V)};z(Ze,L=>{a(Be).description&&L(Ae)})}var it=p(Ze,2),ft=i(it),nt=p(it,2),bt=i(nt),Et=i(bt),A=i(Et),G=p(Et,2),u=i(G),b=p(bt,2);{var E=L=>{var V=u0(),se=i(V),le=i(se),ve=p(se,2),Ie=i(ve),ae=p(ve,2),Y=i(ae);M((fe,X,re)=>{y(le,fe),ve.disabled=a(j)===a(Be).name,y(Ie,X),y(Y,re)},[()=>h("skills.confirmUninstall").replace("{name}",a(Be).name),()=>a(j)===a(Be).name?h("skills.uninstalling"):h("common.yes"),()=>h("common.no")]),te("click",ve,()=>K(a(Be).name)),te("click",ae,()=>{v(C,"")}),g(L,V)},F=L=>{var V=f0(),se=i(V);M(le=>y(se,le),[()=>h("skills.uninstall")]),te("click",V,()=>K(a(Be).name)),g(L,V)};z(b,L=>{a(C)===a(Be).name?L(E):L(F,-1)})}M((L,V)=>{y(Ee,a(Be).name),y(ft,a(Be).location),Qe(Et,1,`rounded-full px-2 py-1 text-xs font-medium ${a(Be).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),y(A,L),y(u,V)},[()=>a(Be).enabled?h("common.enabled"):h("common.disabled"),()=>h("skills.readOnlyState")]),g(at,St)}),g(De,Ge)};z(me,De=>{a(o)?De(st):a(s)?De(lt,1):a(n).length===0?De(et,2):De(tt,-1)})}g(H,U)};z(J,H=>{a(r)==="installed"&&H(Ye)})}var ot=p(J,2);{var _t=H=>{var U=E0(),me=xe(U),st=i(me),lt=i(st);lt.value=lt.__value="github";var et=p(st,2),tt=p(et,2),De=i(tt),Ge=p(me,2);{var at=ue=>{var Ee=p0(),Ze=i(Ee);M(Ae=>y(Ze,Ae),[()=>h("skills.searching")]),g(ue,Ee)},Be=ue=>{var Ee=y0(),Ze=i(Ee);M(()=>y(Ze,a(_))),g(ue,Ee)},St=ue=>{var Ee=b0(),Ze=i(Ee);M(Ae=>y(Ze,Ae),[()=>h("skills.noResults")]),g(ue,Ee)},ge=ue=>{var Ee=S0();Xe(Ee,21,()=>a(c),rt,(Ze,Ae)=>{const it=ce(()=>P(a(Ae).name));var ft=w0(),nt=i(ft),bt=i(nt),Et=i(bt),A=i(Et),G=p(Et,2),u=i(G),b=p(bt,2),E=i(b),F=p(nt,2);{var L=Q=>{var he=h0(),Ne=i(he);M(()=>y(Ne,a(Ae).description)),g(Q,he)};z(F,Q=>{a(Ae).description&&Q(L)})}var V=p(F,2),se=i(V);{var le=Q=>{var he=m0(),Ne=p(i(he));M(()=>y(Ne,` ${a(Ae).stars??""}`)),g(Q,he)};z(se,Q=>{a(Ae).stars>0&&Q(le)})}var ve=p(se,2);{var Ie=Q=>{var he=_0(),Ne=i(he);M(()=>y(Ne,a(Ae).language)),g(Q,he)};z(ve,Q=>{a(Ae).language&&Q(Ie)})}var ae=p(ve,2),Y=i(ae),fe=p(V,2),X=i(fe),re=i(X),ie=p(X,2);{var de=Q=>{var he=x0(),Ne=i(he);M(vt=>y(Ne,vt),[()=>h("skills.installed")]),g(Q,he)},pe=Q=>{var he=k0(),Ne=i(he);M(vt=>{he.disabled=a(T)===a(Ae).url,y(Ne,vt)},[()=>a(T)===a(Ae).url?h("skills.installing"):h("skills.install")]),te("click",he,()=>W(a(Ae).url,a(Ae).name)),g(Q,he)};z(ie,Q=>{a(it)?Q(de):Q(pe,-1)})}M((Q,he,Ne)=>{y(A,a(Ae).name),y(u,`${Q??""} ${a(Ae).owner??""}`),y(E,a(Ae).source),Qe(ae,1,eo(a(Ae).has_license?"text-green-600 dark:text-green-400":"text-yellow-600 dark:text-yellow-400")),y(Y,he),$e(X,"href",a(Ae).url),y(re,Ne)},[()=>h("skills.owner"),()=>a(Ae).has_license?h("skills.licensed"):h("skills.unlicensed"),()=>a(Ae).url.replace("https://github.com/","")]),g(Ze,ft)}),g(ue,Ee)};z(Ge,ue=>{a(f)?ue(at):a(_)?ue(Be,1):a(I)&&a(c).length===0?ue(St,2):a(c).length>0&&ue(ge,3)})}M((ue,Ee)=>{$e(et,"placeholder",ue),tt.disabled=a(f),y(De,Ee)},[()=>h("skills.search"),()=>a(f)?h("skills.searching"):h("skills.searchBtn")]),Rn(st,()=>a(k),ue=>v(k,ue)),te("keydown",et,Se),Br(et,()=>a(w),ue=>v(w,ue)),te("click",tt,N),g(H,U)};z(ot,H=>{a(r)==="discover"&&H(_t)})}M((H,U,me,st)=>{y(Z,H),y(ze,U),Qe(He,1,`rounded-md px-4 py-2 text-sm font-medium transition ${a(r)==="installed"?"bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white":"text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"}`),y(yt,me),Qe(be,1,`rounded-md px-4 py-2 text-sm font-medium transition ${a(r)==="discover"?"bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white":"text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"}`),y(ne,st)},[()=>h("skills.title"),()=>h("common.refresh"),()=>h("skills.tabInstalled"),()=>h("skills.tabDiscover")]),te("click",Oe,()=>{v(o,!0),q()}),te("click",He,()=>{v(r,"installed")}),te("click",be,()=>{v(r,"discover")}),g(e,_e),Pe()}ir(["click","keydown"]);var C0=x("<div> </div>"),M0=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),N0=x('<div class="rounded-lg border border-red-200 bg-red-50 p-4 text-sm text-red-600 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400"> </div>'),T0=x('<div class="rounded-lg border border-gray-200 bg-gray-50 p-8 text-center dark:border-gray-700 dark:bg-gray-800"><!> <p class="text-sm text-gray-500 dark:text-gray-400"> </p></div>'),P0=x('<p class="mb-3 text-sm text-gray-600 dark:text-gray-300"> </p>'),O0=x('<span class="rounded-full bg-sky-100 px-2 py-0.5 text-xs text-sky-700 dark:bg-sky-900/30 dark:text-sky-300"> </span>'),F0=x('<div class="mb-3"><p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400"> </p> <div class="flex flex-wrap gap-1"></div></div>'),I0=x('<span class="rounded-full bg-amber-100 px-2 py-0.5 text-xs text-amber-700 dark:bg-amber-900/30 dark:text-amber-300"> </span>'),L0=x('<div class="mb-3"><p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400"> </p> <div class="flex flex-wrap gap-1"></div></div>'),R0=x('<div class="rounded-lg border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="mb-3 flex items-start justify-between"><div><h3 class="font-semibold text-gray-900 dark:text-gray-100"> </h3> <p class="text-xs text-gray-500 dark:text-gray-400"> </p></div> <div><!> <span class="text-xs"> </span></div></div> <!> <!> <!> <div class="flex justify-end"><button type="button" class="flex items-center gap-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-xs text-gray-700 transition hover:bg-gray-100 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200 dark:hover:bg-gray-600"><!> </button></div></div>'),j0=x('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),H0=x('<!> <section class="space-y-6"><div class="flex items-center justify-between"><div class="flex items-center gap-2"><!> <h2 class="text-2xl font-semibold"> </h2></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!></section>',1);function D0(e,t){Te(t,!0);let r=R(mt([])),n=R(!0),o=R(""),s=R(""),l=R(""),d=R("success");function c(B,Z="success"){v(l,B,!0),v(d,Z,!0),setTimeout(()=>{v(l,"")},3e3)}async function f(){v(n,!0);try{const B=await ht.getPlugins();v(r,Array.isArray(B==null?void 0:B.plugins)?B.plugins:[],!0),v(o,"")}catch{v(r,[],!0),v(o,h("plugins.loadFailed"),!0)}finally{v(n,!1)}}async function _(B){v(s,B,!0);try{await ht.reloadPlugin(B),c(h("plugins.reloadSuccess",{name:B})),await f()}catch(Z){c(h("plugins.reloadFailed")+(Z.message?`: ${Z.message}`:""),"error")}finally{v(s,"")}}function w(B){return typeof B=="string"&&B==="Active"?"text-green-500":typeof B=="object"&&(B!=null&&B.Error)?"text-red-500":"text-yellow-500"}function k(B){return typeof B=="string"&&B==="Active"?h("plugins.statusActive"):typeof B=="object"&&(B!=null&&B.Error)?B.Error:h("common.unknown")}Lt(()=>{f()});var I=H0(),T=xe(I);{var j=B=>{var Z=C0(),ee=i(Z);M(()=>{Qe(Z,1,`fixed right-4 top-4 z-50 rounded-lg px-4 py-2 text-sm font-medium text-white shadow-lg transition ${a(d)==="error"?"bg-red-600":"bg-green-600"}`),y(ee,a(l))}),g(B,Z)};z(T,B=>{a(l)&&B(j)})}var C=p(T,2),O=i(C),q=i(O),K=i(q);So(K,{size:24});var S=p(K,2),m=i(S),N=p(q,2),P=i(N),W=p(O,2);{var Se=B=>{var Z=M0(),ee=i(Z);M(oe=>y(ee,oe),[()=>h("plugins.loading")]),g(B,Z)},_e=B=>{var Z=N0(),ee=i(Z);M(()=>y(ee,a(o))),g(B,Z)},je=B=>{var Z=T0(),ee=i(Z);So(ee,{size:40,class:"mx-auto mb-3 text-gray-400 dark:text-gray-500"});var oe=p(ee,2),Oe=i(oe);M(ze=>y(Oe,ze),[()=>h("plugins.noPlugins")]),g(B,Z)},qe=B=>{var Z=j0();Xe(Z,21,()=>a(r),rt,(ee,oe)=>{var Oe=R0(),ze=i(Oe),ke=i(ze),He=i(ke),yt=i(He),be=p(He,2),ne=i(be),we=p(ke,2),D=i(we);{var J=ge=>{Ad(ge,{size:16})},Ye=ge=>{Ed(ge,{size:16})};z(D,ge=>{typeof a(oe).status=="string"&&a(oe).status==="Active"?ge(J):ge(Ye,-1)})}var ot=p(D,2),_t=i(ot),H=p(ze,2);{var U=ge=>{var ue=P0(),Ee=i(ue);M(()=>y(Ee,a(oe).description)),g(ge,ue)};z(H,ge=>{a(oe).description&&ge(U)})}var me=p(H,2);{var st=ge=>{var ue=F0(),Ee=i(ue),Ze=i(Ee),Ae=p(Ee,2);Xe(Ae,21,()=>a(oe).capabilities,rt,(it,ft)=>{var nt=O0(),bt=i(nt);M(()=>y(bt,a(ft))),g(it,nt)}),M(it=>y(Ze,it),[()=>h("plugins.capabilities")]),g(ge,ue)};z(me,ge=>{var ue;(ue=a(oe).capabilities)!=null&&ue.length&&ge(st)})}var lt=p(me,2);{var et=ge=>{var ue=L0(),Ee=i(ue),Ze=i(Ee),Ae=p(Ee,2);Xe(Ae,21,()=>a(oe).permissions_required,rt,(it,ft)=>{var nt=I0(),bt=i(nt);M(()=>y(bt,a(ft))),g(it,nt)}),M(it=>y(Ze,it),[()=>h("plugins.permissions")]),g(ge,ue)};z(lt,ge=>{var ue;(ue=a(oe).permissions_required)!=null&&ue.length&&ge(et)})}var tt=p(lt,2),De=i(tt),Ge=i(De);{var at=ge=>{Fd(ge,{size:14,class:"animate-spin"})},Be=ge=>{Ps(ge,{size:14})};z(Ge,ge=>{a(s)===a(oe).name?ge(at):ge(Be,-1)})}var St=p(Ge);M((ge,ue,Ee)=>{y(yt,a(oe).name),y(ne,`v${a(oe).version??""}`),Qe(we,1,`flex items-center gap-1 ${ge??""}`),y(_t,ue),De.disabled=a(s)===a(oe).name,y(St,` ${Ee??""}`)},[()=>w(a(oe).status),()=>k(a(oe).status),()=>h("plugins.reload")]),te("click",De,()=>_(a(oe).name)),g(ee,Oe)}),g(B,Z)};z(W,B=>{a(n)?B(Se):a(o)?B(_e,1):a(r).length===0?B(je,2):B(qe,-1)})}M((B,Z)=>{y(m,B),y(P,Z)},[()=>h("plugins.title"),()=>h("common.refresh")]),te("click",N,f),g(e,I),Pe()}ir(["click"]);var U0=x('<button type="button" class="fixed inset-0 z-30 bg-black/30 dark:bg-black/60 lg:hidden"></button>'),z0=x('<button type="button"> </button>'),B0=x('<p class="px-2 py-1 text-xs text-gray-400 dark:text-gray-500">Loading...</p>'),W0=x('<div class="ml-4 mt-1 space-y-1 border-l border-gray-200 pl-3 dark:border-gray-700"><!> <!></div>'),q0=x('<button type="button"> </button> <!>',1),V0=x('<section class="space-y-4"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></section>'),G0=x('<div class="flex min-h-screen"><!> <aside><div class="mb-4 border-b border-gray-200 pb-4 dark:border-gray-700"><p class="text-lg font-semibold"> </p></div> <nav class="space-y-1"></nav></aside> <div class="flex min-w-0 flex-1 flex-col"><header class="sticky top-0 z-20 flex items-center justify-between border-b border-gray-200 bg-white/95 px-4 py-3 backdrop-blur dark:border-gray-700 dark:bg-gray-900/95"><div class="flex items-center gap-3"><button type="button" class="rounded-lg border border-gray-300 px-2 py-1 text-sm text-gray-700 dark:border-gray-700 dark:text-gray-200 lg:hidden"> </button> <h1 class="text-lg font-semibold"> </h1></div> <div class="flex items-center gap-2"><button type="button" aria-label="Toggle theme" class="rounded-lg border border-gray-300 bg-white p-2 text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"><!></button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></header> <main class="flex-1 p-4 sm:p-6"><!></main></div></div>'),K0=x('<div class="min-h-screen bg-gray-50 text-gray-900 dark:bg-gray-900 dark:text-gray-100"><!></div>');function J0(e,t){Te(t,!0);let r=R(mt($s())),n=R(mt(Ra())),o=R(!1),s=R(!0),l=R(mt([])),d=R(!1),c=R(mt(typeof window<"u"?window.location.hash:""));const f=ce(()=>a(n).length>0),_=ce(()=>a(f)&&a(r)==="/"?"/overview":a(r)),w=ce(()=>a(_).startsWith("/chat/")?"/sessions":a(_)),k=ce(()=>a(_)==="/config"),I=ce(()=>a(c).startsWith("#config-section-")?a(c).slice(16):"");function T(ee){try{return decodeURIComponent(ee)}catch{return ee}}const j=ce(()=>a(_).startsWith("/chat/")?T(a(_).slice(6)):"");function C(){localStorage.getItem("prx-console-theme")==="light"?v(s,!1):v(s,!0),O()}function O(){a(s)?document.documentElement.classList.add("dark"):document.documentElement.classList.remove("dark")}function q(){v(s,!a(s)),localStorage.setItem("prx-console-theme",a(s)?"dark":"light"),O()}function K(){v(n,Ra(),!0)}function S(ee){v(r,ee,!0),v(o,!1),v(c,typeof window<"u"?window.location.hash:"",!0)}function m(ee){v(n,ee,!0),$r("/overview",!0)}function N(){As(),v(n,""),$r("/",!0)}function P(ee){$r(ee)}function W(){v(c,window.location.hash,!0)}async function Se(){if(!(!a(f)||a(_)!=="/config"||a(d))){v(d,!0);try{const ee=await ht.getConfig();v(l,Dn(ee),!0)}catch{v(l,Dn(null),!0)}finally{v(d,!1)}}}function _e(ee){Ms(ee),v(o,!1)}Lt(()=>{C();const ee=Fl(S),oe=Oe=>{if(Oe.key==="prx-console-token"){K();return}if(Oe.key===un&&yd(),Oe.key==="prx-console-theme"){const ze=localStorage.getItem("prx-console-theme");v(s,ze!=="light"),O()}};return window.addEventListener("storage",oe),window.addEventListener("hashchange",W),()=>{ee(),window.removeEventListener("storage",oe),window.removeEventListener("hashchange",W)}}),Lt(()=>{if(a(f)&&a(r)==="/"){$r("/overview",!0);return}!a(f)&&a(r)!=="/"&&$r("/",!0)}),Lt(()=>{if(a(k)){Se();return}v(l,[],!0)});var je=K0(),qe=i(je);{var B=ee=>{qd(ee,{onLogin:m})},Z=ee=>{var oe=G0(),Oe=i(oe);{var ze=u=>{var b=U0();M(E=>$e(b,"aria-label",E),[()=>h("app.closeSidebar")]),te("click",b,()=>v(o,!1)),g(u,b)};z(Oe,u=>{a(o)&&u(ze)})}var ke=p(Oe,2),He=i(ke),yt=i(He),be=i(yt),ne=p(He,2);Xe(ne,21,()=>Pl,rt,(u,b)=>{var E=q0(),F=xe(E),L=i(F),V=p(F,2);{var se=le=>{var ve=W0(),Ie=i(ve);Xe(Ie,17,()=>a(l),rt,(fe,X)=>{var re=z0(),ie=i(re);M(()=>{Qe(re,1,`w-full rounded-md px-2 py-1.5 text-left text-xs transition ${a(I)===a(X).groupKey?"bg-sky-50 text-sky-700 dark:bg-sky-500/10 dark:text-sky-300":"text-gray-500 hover:bg-gray-100 hover:text-gray-800 dark:text-gray-400 dark:hover:bg-gray-700 dark:hover:text-gray-100"}`),y(ie,a(X).label)}),te("click",re,()=>_e(a(X).groupKey)),g(fe,re)});var ae=p(Ie,2);{var Y=fe=>{var X=B0();g(fe,X)};z(ae,fe=>{a(d)&&a(l).length===0&&fe(Y)})}g(le,ve)};z(V,le=>{a(b).path==="/config"&&a(k)&&le(se)})}M(le=>{Qe(F,1,`w-full rounded-lg px-3 py-2 text-left text-sm transition ${a(w)===a(b).path?"bg-sky-600 text-white":"text-gray-600 hover:bg-gray-100 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-gray-100"}`),y(L,le)},[()=>h(a(b).labelKey)]),te("click",F,()=>P(a(b).path)),g(u,E)});var we=p(ke,2),D=i(we),J=i(D),Ye=i(J),ot=i(Ye),_t=p(Ye,2),H=i(_t),U=p(J,2),me=i(U),st=i(me);{var lt=u=>{Ud(u,{size:16})},et=u=>{Ld(u,{size:16})};z(st,u=>{a(s)?u(lt):u(et,-1)})}var tt=p(me,2),De=i(tt),Ge=p(tt,2),at=i(Ge),Be=p(D,2),St=i(Be);{var ge=u=>{rc(u,{})},ue=u=>{cc(u,{})},Ee=u=>{Ac(u,{get sessionId(){return a(j)}})},Ze=ce(()=>a(_).startsWith("/chat/")),Ae=u=>{Fc(u,{})},it=u=>{Gu(u,{})},ft=u=>{n0(u,{})},nt=u=>{$0(u,{})},bt=u=>{D0(u,{})},Et=u=>{$u(u,{})},A=u=>{Pu(u,{})},G=u=>{var b=V0(),E=i(b),F=i(E),L=p(E,2),V=i(L);M((se,le)=>{y(F,se),y(V,le)},[()=>h("app.notFound"),()=>h("app.backToOverview")]),te("click",L,()=>P("/overview")),g(u,b)};z(St,u=>{a(_)==="/overview"?u(ge):a(_)==="/sessions"?u(ue,1):a(Ze)?u(Ee,2):a(_)==="/channels"?u(Ae,3):a(_)==="/hooks"?u(it,4):a(_)==="/mcp"?u(ft,5):a(_)==="/skills"?u(nt,6):a(_)==="/plugins"?u(bt,7):a(_)==="/config"?u(Et,8):a(_)==="/logs"?u(A,9):u(G,-1)})}M((u,b,E,F,L)=>{Qe(ke,1,`fixed inset-y-0 left-0 z-40 w-64 border-r border-gray-200 bg-white p-4 transition-transform dark:border-gray-700 dark:bg-gray-800 lg:static lg:translate-x-0 ${a(o)?"translate-x-0":"-translate-x-full"}`),y(be,u),y(ot,b),y(H,E),$e(tt,"aria-label",F),y(De,Qr.lang==="zh"?"中文 / EN":"EN / 中文"),y(at,L)},[()=>h("app.title"),()=>h("app.menu"),()=>h("app.title"),()=>h("app.language"),()=>h("common.logout")]),te("click",Ye,()=>v(o,!a(o))),te("click",me,q),te("click",tt,function(...u){na==null||na.apply(this,u)}),te("click",Ge,N),g(ee,oe)};z(qe,ee=>{a(f)?ee(Z,-1):ee(B)})}g(e,je),Pe()}ir(["click"]);il(J0,{target:document.getElementById("app")});
