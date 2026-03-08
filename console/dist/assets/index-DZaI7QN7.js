var Eo=Object.defineProperty;var Zn=e=>{throw TypeError(e)};var $o=(e,t,r)=>t in e?Eo(e,t,{enumerable:!0,configurable:!0,writable:!0,value:r}):e[t]=r;var Zt=(e,t,r)=>$o(e,typeof t!="symbol"?t+"":t,r),un=(e,t,r)=>t.has(e)||Zn("Cannot "+r);var $=(e,t,r)=>(un(e,t,"read from private field"),r?r.call(e):t.get(e)),Be=(e,t,r)=>t.has(e)?Zn("Cannot add the same private member more than once"):t instanceof WeakSet?t.add(e):t.set(e,r),Oe=(e,t,r,n)=>(un(e,t,"write to private field"),n?n.call(e,r):t.set(e,r),r),At=(e,t,r)=>(un(e,t,"access private method"),r);(function(){const t=document.createElement("link").relList;if(t&&t.supports&&t.supports("modulepreload"))return;for(const s of document.querySelectorAll('link[rel="modulepreload"]'))n(s);new MutationObserver(s=>{for(const o of s)if(o.type==="childList")for(const l of o.addedNodes)l.tagName==="LINK"&&l.rel==="modulepreload"&&n(l)}).observe(document,{childList:!0,subtree:!0});function r(s){const o={};return s.integrity&&(o.integrity=s.integrity),s.referrerPolicy&&(o.referrerPolicy=s.referrerPolicy),s.crossOrigin==="use-credentials"?o.credentials="include":s.crossOrigin==="anonymous"?o.credentials="omit":o.credentials="same-origin",o}function n(s){if(s.ep)return;s.ep=!0;const o=r(s);fetch(s.href,o)}})();const _n=!1;var jn=Array.isArray,Co=Array.prototype.indexOf,ca=Array.prototype.includes,Za=Array.from,Mo=Object.defineProperty,Mr=Object.getOwnPropertyDescriptor,No=Object.getOwnPropertyDescriptors,Po=Object.prototype,To=Array.prototype,ks=Object.getPrototypeOf,es=Object.isExtensible;function Aa(e){return typeof e=="function"}const me=()=>{};function Oo(e){for(var t=0;t<e.length;t++)e[t]()}function ws(){var e,t,r=new Promise((n,s)=>{e=n,t=s});return{promise:r,resolve:e,reject:t}}function Ma(e,t){if(Array.isArray(e))return e;if(!(Symbol.iterator in e))return Array.from(e);const r=[];for(const n of e)if(r.push(n),r.length===t)break;return r}const Ot=2,ya=4,ua=8,en=1<<24,Lr=16,sr=32,Xr=64,xn=128,Jt=512,Nt=1024,Pt=2048,nr=4096,It=8192,vr=16384,ha=32768,kr=65536,ts=1<<17,Io=1<<18,ma=1<<19,Lo=1<<20,ur=1<<25,Gr=65536,kn=1<<21,Dn=1<<22,Nr=1<<23,Pr=Symbol("$state"),Ss=Symbol("legacy props"),Fo=Symbol(""),Rr=new class extends Error{constructor(){super(...arguments);Zt(this,"name","StaleReactionError");Zt(this,"message","The reaction that called `getAbortSignal()` was re-run or destroyed")}};var ms;const zn=!!((ms=globalThis.document)!=null&&ms.contentType)&&globalThis.document.contentType.includes("xml");function As(e){throw new Error("https://svelte.dev/e/lifecycle_outside_component")}function Ro(){throw new Error("https://svelte.dev/e/async_derived_orphan")}function jo(e,t,r){throw new Error("https://svelte.dev/e/each_key_duplicate")}function Do(e){throw new Error("https://svelte.dev/e/effect_in_teardown")}function zo(){throw new Error("https://svelte.dev/e/effect_in_unowned_derived")}function Ho(e){throw new Error("https://svelte.dev/e/effect_orphan")}function Uo(){throw new Error("https://svelte.dev/e/effect_update_depth_exceeded")}function Bo(e){throw new Error("https://svelte.dev/e/props_invalid_value")}function Wo(){throw new Error("https://svelte.dev/e/state_descriptors_fixed")}function Vo(){throw new Error("https://svelte.dev/e/state_prototype_fixed")}function qo(){throw new Error("https://svelte.dev/e/state_unsafe_mutation")}function Ko(){throw new Error("https://svelte.dev/e/svelte_boundary_reset_onerror")}const Go=1,Jo=2,Es=4,Yo=8,Xo=16,Qo=1,Zo=4,ei=8,ti=16,ri=1,ai=2,$t=Symbol(),$s="http://www.w3.org/1999/xhtml",Cs="http://www.w3.org/2000/svg",ni="http://www.w3.org/1998/Math/MathML",si="@attach";function oi(){console.warn("https://svelte.dev/e/select_multiple_invalid_value")}function ii(){console.warn("https://svelte.dev/e/svelte_boundary_reset_noop")}function Ms(e){return e===this.v}function li(e,t){return e!=e?t==t:e!==t||e!==null&&typeof e=="object"||typeof e=="function"}function Ns(e){return!li(e,this.v)}let di=!1,Ht=null;function fa(e){Ht=e}function $e(e,t=!1,r){Ht={p:Ht,i:!1,c:null,e:null,s:e,x:null,l:null}}function Ce(e){var t=Ht,r=t.e;if(r!==null){t.e=null;for(var n of r)Xs(n)}return t.i=!0,Ht=t.p,{}}function Ps(){return!0}let jr=[];function Ts(){var e=jr;jr=[],Oo(e)}function gr(e){if(jr.length===0&&!Pa){var t=jr;queueMicrotask(()=>{t===jr&&Ts()})}jr.push(e)}function ci(){for(;jr.length>0;)Ts()}function Os(e){var t=Ge;if(t===null)return De.f|=Nr,e;if(!(t.f&ha)&&!(t.f&ya))throw e;Cr(e,t)}function Cr(e,t){for(;t!==null;){if(t.f&xn){if(!(t.f&ha))throw e;try{t.b.error(e);return}catch(r){e=r}}t=t.parent}throw e}const ui=-7169;function xt(e,t){e.f=e.f&ui|t}function Hn(e){e.f&Jt||e.deps===null?xt(e,Nt):xt(e,nr)}function Is(e){if(e!==null)for(const t of e)!(t.f&Ot)||!(t.f&Gr)||(t.f^=Gr,Is(t.deps))}function Ls(e,t,r){e.f&Pt?t.add(e):e.f&nr&&r.add(e),Is(e.deps),xt(e,Nt)}const Ba=new Set;let _e=null,Ja=null,Mt=null,jt=[],tn=null,Pa=!1,va=null,fi=1;var Ar,ra,Ur,aa,na,sa,Er,ir,oa,Bt,wn,Sn,An,En;const Qn=class Qn{constructor(){Be(this,Bt);Zt(this,"id",fi++);Zt(this,"current",new Map);Zt(this,"previous",new Map);Be(this,Ar,new Set);Be(this,ra,new Set);Be(this,Ur,0);Be(this,aa,0);Be(this,na,null);Be(this,sa,new Set);Be(this,Er,new Set);Be(this,ir,new Map);Zt(this,"is_fork",!1);Be(this,oa,!1)}skip_effect(t){$(this,ir).has(t)||$(this,ir).set(t,{d:[],m:[]})}unskip_effect(t){var r=$(this,ir).get(t);if(r){$(this,ir).delete(t);for(var n of r.d)xt(n,Pt),fr(n);for(n of r.m)xt(n,nr),fr(n)}}process(t){var s;jt=[],this.apply();var r=va=[],n=[];for(const o of t)At(this,Bt,Sn).call(this,o,r,n);if(va=null,At(this,Bt,wn).call(this)){At(this,Bt,An).call(this,n),At(this,Bt,An).call(this,r);for(const[o,l]of $(this,ir))Ds(o,l)}else{Ja=this,_e=null;for(const o of $(this,Ar))o(this);$(this,Ar).clear(),$(this,Ur)===0&&At(this,Bt,En).call(this),rs(n),rs(r),$(this,sa).clear(),$(this,Er).clear(),Ja=null,(s=$(this,na))==null||s.resolve()}Mt=null}capture(t,r){r!==$t&&!this.previous.has(t)&&this.previous.set(t,r),t.f&Nr||(this.current.set(t,t.v),Mt==null||Mt.set(t,t.v))}activate(){_e=this,this.apply()}deactivate(){_e===this&&(_e=null,Mt=null)}flush(){var t;if(jt.length>0)_e=this,Fs();else if($(this,Ur)===0&&!this.is_fork){for(const r of $(this,Ar))r(this);$(this,Ar).clear(),At(this,Bt,En).call(this),(t=$(this,na))==null||t.resolve()}this.deactivate()}discard(){for(const t of $(this,ra))t(this);$(this,ra).clear()}increment(t){Oe(this,Ur,$(this,Ur)+1),t&&Oe(this,aa,$(this,aa)+1)}decrement(t){Oe(this,Ur,$(this,Ur)-1),t&&Oe(this,aa,$(this,aa)-1),!$(this,oa)&&(Oe(this,oa,!0),gr(()=>{Oe(this,oa,!1),At(this,Bt,wn).call(this)?jt.length>0&&this.flush():this.revive()}))}revive(){for(const t of $(this,sa))$(this,Er).delete(t),xt(t,Pt),fr(t);for(const t of $(this,Er))xt(t,nr),fr(t);this.flush()}oncommit(t){$(this,Ar).add(t)}ondiscard(t){$(this,ra).add(t)}settled(){return($(this,na)??Oe(this,na,ws())).promise}static ensure(){if(_e===null){const t=_e=new Qn;Ba.add(_e),Pa||gr(()=>{_e===t&&t.flush()})}return _e}apply(){}};Ar=new WeakMap,ra=new WeakMap,Ur=new WeakMap,aa=new WeakMap,na=new WeakMap,sa=new WeakMap,Er=new WeakMap,ir=new WeakMap,oa=new WeakMap,Bt=new WeakSet,wn=function(){return this.is_fork||$(this,aa)>0},Sn=function(t,r,n){t.f^=Nt;for(var s=t.first;s!==null;){var o=s.f,l=(o&(sr|Xr))!==0,d=l&&(o&Nt)!==0,c=(o&It)!==0,u=d||$(this,ir).has(s);if(!u&&s.fn!==null){l?c||(s.f^=Nt):o&ya?r.push(s):o&(ua|en)&&c?n.push(s):Ha(s)&&(pa(s),o&Lr&&($(this,Er).add(s),c&&xt(s,Pt)));var m=s.first;if(m!==null){s=m;continue}}for(;s!==null;){var x=s.next;if(x!==null){s=x;break}s=s.parent}}},An=function(t){for(var r=0;r<t.length;r+=1)Ls(t[r],$(this,sa),$(this,Er))},En=function(){var o;if(Ba.size>1){this.previous.clear();var t=_e,r=Mt,n=!0;for(const l of Ba){if(l===this){n=!1;continue}const d=[];for(const[u,m]of this.current){if(l.current.has(u))if(n&&m!==l.current.get(u))l.current.set(u,m);else continue;d.push(u)}if(d.length===0)continue;const c=[...l.current.keys()].filter(u=>!this.current.has(u));if(c.length>0){var s=jt;jt=[];const u=new Set,m=new Map;for(const x of d)Rs(x,c,u,m);if(jt.length>0){_e=l,l.apply();for(const x of jt)At(o=l,Bt,Sn).call(o,x,[],[]);l.deactivate()}jt=s}}_e=t,Mt=r}$(this,ir).clear(),Ba.delete(this)};let Tr=Qn;function vi(e){var t=Pa;Pa=!0;try{for(var r;;){if(ci(),jt.length===0&&(_e==null||_e.flush(),jt.length===0))return tn=null,r;Fs()}}finally{Pa=t}}function Fs(){var e=null;try{for(var t=0;jt.length>0;){var r=Tr.ensure();if(t++>1e3){var n,s;gi()}r.process(jt),Or.clear()}}finally{jt=[],tn=null,va=null}}function gi(){try{Uo()}catch(e){Cr(e,tn)}}let er=null;function rs(e){var t=e.length;if(t!==0){for(var r=0;r<t;){var n=e[r++];if(!(n.f&(vr|It))&&Ha(n)&&(er=new Set,pa(n),n.deps===null&&n.first===null&&n.nodes===null&&n.teardown===null&&n.ac===null&&to(n),(er==null?void 0:er.size)>0)){Or.clear();for(const s of er){if(s.f&(vr|It))continue;const o=[s];let l=s.parent;for(;l!==null;)er.has(l)&&(er.delete(l),o.push(l)),l=l.parent;for(let d=o.length-1;d>=0;d--){const c=o[d];c.f&(vr|It)||pa(c)}}er.clear()}}er=null}}function Rs(e,t,r,n){if(!r.has(e)&&(r.add(e),e.reactions!==null))for(const s of e.reactions){const o=s.f;o&Ot?Rs(s,t,r,n):o&(Dn|Lr)&&!(o&Pt)&&js(s,t,n)&&(xt(s,Pt),fr(s))}}function js(e,t,r){const n=r.get(e);if(n!==void 0)return n;if(e.deps!==null)for(const s of e.deps){if(ca.call(t,s))return!0;if(s.f&Ot&&js(s,t,r))return r.set(s,!0),!0}return r.set(e,!1),!1}function fr(e){var t=tn=e,r=t.b;if(r!=null&&r.is_pending&&e.f&(ya|ua|en)&&!(e.f&ha)){r.defer_effect(e);return}for(;t.parent!==null;){t=t.parent;var n=t.f;if(va!==null&&t===Ge&&!(e.f&ua))return;if(n&(Xr|sr)){if(!(n&Nt))return;t.f^=Nt}}jt.push(t)}function Ds(e,t){if(!(e.f&sr&&e.f&Nt)){e.f&Pt?t.d.push(e):e.f&nr&&t.m.push(e),xt(e,Nt);for(var r=e.first;r!==null;)Ds(r,t),r=r.next}}function pi(e){let t=0,r=Jr(0),n;return()=>{Wn()&&(a(r),Vn(()=>(t===0&&(n=xa(()=>e(()=>Ta(r)))),t+=1,()=>{gr(()=>{t-=1,t===0&&(n==null||n(),n=void 0,Ta(r))})})))}}var bi=kr|ma;function yi(e,t,r,n){new hi(e,t,r,n)}var Gt,Rn,lr,Br,Rt,dr,Vt,tr,hr,Wr,$r,ia,la,da,mr,Xa,Et,mi,_i,xi,$n,qa,Ka,Cn;class hi{constructor(t,r,n,s){Be(this,Et);Zt(this,"parent");Zt(this,"is_pending",!1);Zt(this,"transform_error");Be(this,Gt);Be(this,Rn,null);Be(this,lr);Be(this,Br);Be(this,Rt);Be(this,dr,null);Be(this,Vt,null);Be(this,tr,null);Be(this,hr,null);Be(this,Wr,0);Be(this,$r,0);Be(this,ia,!1);Be(this,la,new Set);Be(this,da,new Set);Be(this,mr,null);Be(this,Xa,pi(()=>(Oe(this,mr,Jr($(this,Wr))),()=>{Oe(this,mr,null)})));var o;Oe(this,Gt,t),Oe(this,lr,r),Oe(this,Br,l=>{var d=Ge;d.b=this,d.f|=xn,n(l)}),this.parent=Ge.b,this.transform_error=s??((o=this.parent)==null?void 0:o.transform_error)??(l=>l),Oe(this,Rt,_a(()=>{At(this,Et,$n).call(this)},bi))}defer_effect(t){Ls(t,$(this,la),$(this,da))}is_rendered(){return!this.is_pending&&(!this.parent||this.parent.is_rendered())}has_pending_snippet(){return!!$(this,lr).pending}update_pending_count(t){At(this,Et,Cn).call(this,t),Oe(this,Wr,$(this,Wr)+t),!(!$(this,mr)||$(this,ia))&&(Oe(this,ia,!0),gr(()=>{Oe(this,ia,!1),$(this,mr)&&ga($(this,mr),$(this,Wr))}))}get_effect_pending(){return $(this,Xa).call(this),a($(this,mr))}error(t){var r=$(this,lr).onerror;let n=$(this,lr).failed;if(!r&&!n)throw t;$(this,dr)&&(Tt($(this,dr)),Oe(this,dr,null)),$(this,Vt)&&(Tt($(this,Vt)),Oe(this,Vt,null)),$(this,tr)&&(Tt($(this,tr)),Oe(this,tr,null));var s=!1,o=!1;const l=()=>{if(s){ii();return}s=!0,o&&Ko(),$(this,tr)!==null&&qr($(this,tr),()=>{Oe(this,tr,null)}),At(this,Et,Ka).call(this,()=>{Tr.ensure(),At(this,Et,$n).call(this)})},d=c=>{try{o=!0,r==null||r(c,l),o=!1}catch(u){Cr(u,$(this,Rt)&&$(this,Rt).parent)}n&&Oe(this,tr,At(this,Et,Ka).call(this,()=>{Tr.ensure();try{return zt(()=>{var u=Ge;u.b=this,u.f|=xn,n($(this,Gt),()=>c,()=>l)})}catch(u){return Cr(u,$(this,Rt).parent),null}}))};gr(()=>{var c;try{c=this.transform_error(t)}catch(u){Cr(u,$(this,Rt)&&$(this,Rt).parent);return}c!==null&&typeof c=="object"&&typeof c.then=="function"?c.then(d,u=>Cr(u,$(this,Rt)&&$(this,Rt).parent)):d(c)})}}Gt=new WeakMap,Rn=new WeakMap,lr=new WeakMap,Br=new WeakMap,Rt=new WeakMap,dr=new WeakMap,Vt=new WeakMap,tr=new WeakMap,hr=new WeakMap,Wr=new WeakMap,$r=new WeakMap,ia=new WeakMap,la=new WeakMap,da=new WeakMap,mr=new WeakMap,Xa=new WeakMap,Et=new WeakSet,mi=function(){try{Oe(this,dr,zt(()=>$(this,Br).call(this,$(this,Gt))))}catch(t){this.error(t)}},_i=function(t){const r=$(this,lr).failed;r&&Oe(this,tr,zt(()=>{r($(this,Gt),()=>t,()=>()=>{})}))},xi=function(){const t=$(this,lr).pending;t&&(this.is_pending=!0,Oe(this,Vt,zt(()=>t($(this,Gt)))),gr(()=>{var r=Oe(this,hr,document.createDocumentFragment()),n=_r();r.append(n),Oe(this,dr,At(this,Et,Ka).call(this,()=>(Tr.ensure(),zt(()=>$(this,Br).call(this,n))))),$(this,$r)===0&&($(this,Gt).before(r),Oe(this,hr,null),qr($(this,Vt),()=>{Oe(this,Vt,null)}),At(this,Et,qa).call(this))}))},$n=function(){try{if(this.is_pending=this.has_pending_snippet(),Oe(this,$r,0),Oe(this,Wr,0),Oe(this,dr,zt(()=>{$(this,Br).call(this,$(this,Gt))})),$(this,$r)>0){var t=Oe(this,hr,document.createDocumentFragment());Gn($(this,dr),t);const r=$(this,lr).pending;Oe(this,Vt,zt(()=>r($(this,Gt))))}else At(this,Et,qa).call(this)}catch(r){this.error(r)}},qa=function(){this.is_pending=!1;for(const t of $(this,la))xt(t,Pt),fr(t);for(const t of $(this,da))xt(t,nr),fr(t);$(this,la).clear(),$(this,da).clear()},Ka=function(t){var r=Ge,n=De,s=Ht;pr($(this,Rt)),Xt($(this,Rt)),fa($(this,Rt).ctx);try{return t()}catch(o){return Os(o),null}finally{pr(r),Xt(n),fa(s)}},Cn=function(t){var r;if(!this.has_pending_snippet()){this.parent&&At(r=this.parent,Et,Cn).call(r,t);return}Oe(this,$r,$(this,$r)+t),$(this,$r)===0&&(At(this,Et,qa).call(this),$(this,Vt)&&qr($(this,Vt),()=>{Oe(this,Vt,null)}),$(this,hr)&&($(this,Gt).before($(this,hr)),Oe(this,hr,null)))};function zs(e,t,r,n){const s=rn;var o=e.filter(x=>!x.settled);if(r.length===0&&o.length===0){n(t.map(s));return}var l=Ge,d=ki(),c=o.length===1?o[0].promise:o.length>1?Promise.all(o.map(x=>x.promise)):null;function u(x){d();try{n(x)}catch(w){l.f&vr||Cr(w,l)}Mn()}if(r.length===0){c.then(()=>u(t.map(s)));return}function m(){d(),Promise.all(r.map(x=>Si(x))).then(x=>u([...t.map(s),...x])).catch(x=>Cr(x,l))}c?c.then(m):m()}function ki(){var e=Ge,t=De,r=Ht,n=_e;return function(o=!0){pr(e),Xt(t),fa(r),o&&(n==null||n.activate())}}function Mn(e=!0){pr(null),Xt(null),fa(null),e&&(_e==null||_e.deactivate())}function wi(){var e=Ge.b,t=_e,r=e.is_rendered();return e.update_pending_count(1),t.increment(r),()=>{e.update_pending_count(-1),t.decrement(r)}}function rn(e){var t=Ot|Pt,r=De!==null&&De.f&Ot?De:null;return Ge!==null&&(Ge.f|=ma),{ctx:Ht,deps:null,effects:null,equals:Ms,f:t,fn:e,reactions:null,rv:0,v:$t,wv:0,parent:r??Ge,ac:null}}function Si(e,t,r){Ge===null&&Ro();var s=void 0,o=Jr($t),l=!De,d=new Map;return ji(()=>{var w;var c=ws();s=c.promise;try{Promise.resolve(e()).then(c.resolve,c.reject).finally(Mn)}catch(I){c.reject(I),Mn()}var u=_e;if(l){var m=wi();(w=d.get(u))==null||w.reject(Rr),d.delete(u),d.set(u,c)}const x=(I,T=void 0)=>{if(u.activate(),T)T!==Rr&&(o.f|=Nr,ga(o,T));else{o.f&Nr&&(o.f^=Nr),ga(o,I);for(const[R,N]of d){if(d.delete(R),R===u)break;N.reject(Rr)}}m&&m()};c.promise.then(x,I=>x(null,I||"unknown"))}),nn(()=>{for(const c of d.values())c.reject(Rr)}),new Promise(c=>{function u(m){function x(){m===s?c(o):u(s)}m.then(x,x)}u(s)})}function re(e){const t=rn(e);return no(t),t}function Hs(e){const t=rn(e);return t.equals=Ns,t}function Ai(e){var t=e.effects;if(t!==null){e.effects=null;for(var r=0;r<t.length;r+=1)Tt(t[r])}}function Ei(e){for(var t=e.parent;t!==null;){if(!(t.f&Ot))return t.f&vr?null:t;t=t.parent}return null}function Un(e){var t,r=Ge;pr(Ei(e));try{e.f&=~Gr,Ai(e),t=lo(e)}finally{pr(r)}return t}function Us(e){var t=Un(e);if(!e.equals(t)&&(e.wv=oo(),(!(_e!=null&&_e.is_fork)||e.deps===null)&&(e.v=t,e.deps===null))){xt(e,Nt);return}Ir||(Mt!==null?(Wn()||_e!=null&&_e.is_fork)&&Mt.set(e,t):Hn(e))}function $i(e){var t,r;if(e.effects!==null)for(const n of e.effects)(n.teardown||n.ac)&&((t=n.teardown)==null||t.call(n),(r=n.ac)==null||r.abort(Rr),n.teardown=me,n.ac=null,Ia(n,0),qn(n))}function Bs(e){if(e.effects!==null)for(const t of e.effects)t.teardown&&pa(t)}let Nn=new Set;const Or=new Map;let Ws=!1;function Jr(e,t){var r={f:0,v:e,reactions:null,equals:Ms,rv:0,wv:0};return r}function F(e,t){const r=Jr(e);return no(r),r}function Ci(e,t=!1,r=!0){const n=Jr(e);return t||(n.equals=Ns),n}function y(e,t,r=!1){De!==null&&(!ar||De.f&ts)&&Ps()&&De.f&(Ot|Lr|Dn|ts)&&(Yt===null||!ca.call(Yt,e))&&qo();let n=r?kt(t):t;return ga(e,n)}function ga(e,t){if(!e.equals(t)){var r=e.v;Ir?Or.set(e,t):Or.set(e,r),e.v=t;var n=Tr.ensure();if(n.capture(e,r),e.f&Ot){const s=e;e.f&Pt&&Un(s),Hn(s)}e.wv=oo(),Vs(e,Pt),Ge!==null&&Ge.f&Nt&&!(Ge.f&(sr|Xr))&&(Kt===null?zi([e]):Kt.push(e)),!n.is_fork&&Nn.size>0&&!Ws&&Mi()}return t}function Mi(){Ws=!1;for(const e of Nn)e.f&Nt&&xt(e,nr),Ha(e)&&pa(e);Nn.clear()}function Ta(e){y(e,e.v+1)}function Vs(e,t){var r=e.reactions;if(r!==null)for(var n=r.length,s=0;s<n;s++){var o=r[s],l=o.f,d=(l&Pt)===0;if(d&&xt(o,t),l&Ot){var c=o;Mt==null||Mt.delete(c),l&Gr||(l&Jt&&(o.f|=Gr),Vs(c,nr))}else d&&(l&Lr&&er!==null&&er.add(o),fr(o))}}function kt(e){if(typeof e!="object"||e===null||Pr in e)return e;const t=ks(e);if(t!==Po&&t!==To)return e;var r=new Map,n=jn(e),s=F(0),o=Kr,l=d=>{if(Kr===o)return d();var c=De,u=Kr;Xt(null),is(o);var m=d();return Xt(c),is(u),m};return n&&r.set("length",F(e.length)),new Proxy(e,{defineProperty(d,c,u){(!("value"in u)||u.configurable===!1||u.enumerable===!1||u.writable===!1)&&Wo();var m=r.get(c);return m===void 0?l(()=>{var x=F(u.value);return r.set(c,x),x}):y(m,u.value,!0),!0},deleteProperty(d,c){var u=r.get(c);if(u===void 0){if(c in d){const m=l(()=>F($t));r.set(c,m),Ta(s)}}else y(u,$t),Ta(s);return!0},get(d,c,u){var I;if(c===Pr)return e;var m=r.get(c),x=c in d;if(m===void 0&&(!x||(I=Mr(d,c))!=null&&I.writable)&&(m=l(()=>{var T=kt(x?d[c]:$t),R=F(T);return R}),r.set(c,m)),m!==void 0){var w=a(m);return w===$t?void 0:w}return Reflect.get(d,c,u)},getOwnPropertyDescriptor(d,c){var u=Reflect.getOwnPropertyDescriptor(d,c);if(u&&"value"in u){var m=r.get(c);m&&(u.value=a(m))}else if(u===void 0){var x=r.get(c),w=x==null?void 0:x.v;if(x!==void 0&&w!==$t)return{enumerable:!0,configurable:!0,value:w,writable:!0}}return u},has(d,c){var w;if(c===Pr)return!0;var u=r.get(c),m=u!==void 0&&u.v!==$t||Reflect.has(d,c);if(u!==void 0||Ge!==null&&(!m||(w=Mr(d,c))!=null&&w.writable)){u===void 0&&(u=l(()=>{var I=m?kt(d[c]):$t,T=F(I);return T}),r.set(c,u));var x=a(u);if(x===$t)return!1}return m},set(d,c,u,m){var q;var x=r.get(c),w=c in d;if(n&&c==="length")for(var I=u;I<x.v;I+=1){var T=r.get(I+"");T!==void 0?y(T,$t):I in d&&(T=l(()=>F($t)),r.set(I+"",T))}if(x===void 0)(!w||(q=Mr(d,c))!=null&&q.writable)&&(x=l(()=>F(void 0)),y(x,kt(u)),r.set(c,x));else{w=x.v!==$t;var R=l(()=>kt(u));y(x,R)}var N=Reflect.getOwnPropertyDescriptor(d,c);if(N!=null&&N.set&&N.set.call(m,u),!w){if(n&&typeof c=="string"){var O=r.get("length"),Z=Number(c);Number.isInteger(Z)&&Z>=O.v&&y(O,Z+1)}Ta(s)}return!0},ownKeys(d){a(s);var c=Reflect.ownKeys(d).filter(x=>{var w=r.get(x);return w===void 0||w.v!==$t});for(var[u,m]of r)m.v!==$t&&!(u in d)&&c.push(u);return c},setPrototypeOf(){Vo()}})}function as(e){try{if(e!==null&&typeof e=="object"&&Pr in e)return e[Pr]}catch{}return e}function Ni(e,t){return Object.is(as(e),as(t))}var ns,qs,Ks,Gs;function Pi(){if(ns===void 0){ns=window,qs=/Firefox/.test(navigator.userAgent);var e=Element.prototype,t=Node.prototype,r=Text.prototype;Ks=Mr(t,"firstChild").get,Gs=Mr(t,"nextSibling").get,es(e)&&(e.__click=void 0,e.__className=void 0,e.__attributes=null,e.__style=void 0,e.__e=void 0),es(r)&&(r.__t=void 0)}}function _r(e=""){return document.createTextNode(e)}function xr(e){return Ks.call(e)}function za(e){return Gs.call(e)}function i(e,t){return xr(e)}function xe(e,t=!1){{var r=xr(e);return r instanceof Comment&&r.data===""?za(r):r}}function g(e,t=1,r=!1){let n=e;for(;t--;)n=za(n);return n}function Ti(e){e.textContent=""}function Js(){return!1}function Bn(e,t,r){return document.createElementNS(t??$s,e,void 0)}function Oi(e,t){if(t){const r=document.body;e.autofocus=!0,gr(()=>{document.activeElement===r&&e.focus()})}}let ss=!1;function Ii(){ss||(ss=!0,document.addEventListener("reset",e=>{Promise.resolve().then(()=>{var t;if(!e.defaultPrevented)for(const r of e.target.elements)(t=r.__on_r)==null||t.call(r)})},{capture:!0}))}function an(e){var t=De,r=Ge;Xt(null),pr(null);try{return e()}finally{Xt(t),pr(r)}}function Ys(e,t,r,n=r){e.addEventListener(t,()=>an(r));const s=e.__on_r;s?e.__on_r=()=>{s(),n(!0)}:e.__on_r=()=>n(!0),Ii()}function Li(e){Ge===null&&(De===null&&Ho(),zo()),Ir&&Do()}function Fi(e,t){var r=t.last;r===null?t.last=t.first=e:(r.next=e,e.prev=r,t.last=e)}function br(e,t){var r=Ge;r!==null&&r.f&It&&(e|=It);var n={ctx:Ht,deps:null,nodes:null,f:e|Pt|Jt,first:null,fn:t,last:null,next:null,parent:r,b:r&&r.b,prev:null,teardown:null,wv:0,ac:null},s=n;if(e&ya)va!==null?va.push(n):fr(n);else if(t!==null){try{pa(n)}catch(l){throw Tt(n),l}s.deps===null&&s.teardown===null&&s.nodes===null&&s.first===s.last&&!(s.f&ma)&&(s=s.first,e&Lr&&e&kr&&s!==null&&(s.f|=kr))}if(s!==null&&(s.parent=r,r!==null&&Fi(s,r),De!==null&&De.f&Ot&&!(e&Xr))){var o=De;(o.effects??(o.effects=[])).push(s)}return n}function Wn(){return De!==null&&!ar}function nn(e){const t=br(ua,null);return xt(t,Nt),t.teardown=e,t}function Ut(e){Li();var t=Ge.f,r=!De&&(t&sr)!==0&&(t&ha)===0;if(r){var n=Ht;(n.e??(n.e=[])).push(e)}else return Xs(e)}function Xs(e){return br(ya|Lo,e)}function Ri(e){Tr.ensure();const t=br(Xr|ma,e);return(r={})=>new Promise(n=>{r.outro?qr(t,()=>{Tt(t),n(void 0)}):(Tt(t),n(void 0))})}function sn(e){return br(ya,e)}function ji(e){return br(Dn|ma,e)}function Vn(e,t=0){return br(ua|t,e)}function P(e,t=[],r=[],n=[]){zs(n,t,r,s=>{br(ua,()=>e(...s.map(a)))})}function _a(e,t=0){var r=br(Lr|t,e);return r}function Qs(e,t=0){var r=br(en|t,e);return r}function zt(e){return br(sr|ma,e)}function Zs(e){var t=e.teardown;if(t!==null){const r=Ir,n=De;os(!0),Xt(null);try{t.call(null)}finally{os(r),Xt(n)}}}function qn(e,t=!1){var r=e.first;for(e.first=e.last=null;r!==null;){const s=r.ac;s!==null&&an(()=>{s.abort(Rr)});var n=r.next;r.f&Xr?r.parent=null:Tt(r,t),r=n}}function Di(e){for(var t=e.first;t!==null;){var r=t.next;t.f&sr||Tt(t),t=r}}function Tt(e,t=!0){var r=!1;(t||e.f&Io)&&e.nodes!==null&&e.nodes.end!==null&&(eo(e.nodes.start,e.nodes.end),r=!0),qn(e,t&&!r),Ia(e,0),xt(e,vr);var n=e.nodes&&e.nodes.t;if(n!==null)for(const o of n)o.stop();Zs(e);var s=e.parent;s!==null&&s.first!==null&&to(e),e.next=e.prev=e.teardown=e.ctx=e.deps=e.fn=e.nodes=e.ac=null}function eo(e,t){for(;e!==null;){var r=e===t?null:za(e);e.remove(),e=r}}function to(e){var t=e.parent,r=e.prev,n=e.next;r!==null&&(r.next=n),n!==null&&(n.prev=r),t!==null&&(t.first===e&&(t.first=n),t.last===e&&(t.last=r))}function qr(e,t,r=!0){var n=[];ro(e,n,!0);var s=()=>{r&&Tt(e),t&&t()},o=n.length;if(o>0){var l=()=>--o||s();for(var d of n)d.out(l)}else s()}function ro(e,t,r){if(!(e.f&It)){e.f^=It;var n=e.nodes&&e.nodes.t;if(n!==null)for(const d of n)(d.is_global||r)&&t.push(d);for(var s=e.first;s!==null;){var o=s.next,l=(s.f&kr)!==0||(s.f&sr)!==0&&(e.f&Lr)!==0;ro(s,t,l?r:!1),s=o}}}function Kn(e){ao(e,!0)}function ao(e,t){if(e.f&It){e.f^=It;for(var r=e.first;r!==null;){var n=r.next,s=(r.f&kr)!==0||(r.f&sr)!==0;ao(r,s?t:!1),r=n}var o=e.nodes&&e.nodes.t;if(o!==null)for(const l of o)(l.is_global||t)&&l.in()}}function Gn(e,t){if(e.nodes)for(var r=e.nodes.start,n=e.nodes.end;r!==null;){var s=r===n?null:za(r);t.append(r),r=s}}let Ga=!1,Ir=!1;function os(e){Ir=e}let De=null,ar=!1;function Xt(e){De=e}let Ge=null;function pr(e){Ge=e}let Yt=null;function no(e){De!==null&&(Yt===null?Yt=[e]:Yt.push(e))}let Dt=null,Wt=0,Kt=null;function zi(e){Kt=e}let so=1,Dr=0,Kr=Dr;function is(e){Kr=e}function oo(){return++so}function Ha(e){var t=e.f;if(t&Pt)return!0;if(t&Ot&&(e.f&=~Gr),t&nr){for(var r=e.deps,n=r.length,s=0;s<n;s++){var o=r[s];if(Ha(o)&&Us(o),o.wv>e.wv)return!0}t&Jt&&Mt===null&&xt(e,Nt)}return!1}function io(e,t,r=!0){var n=e.reactions;if(n!==null&&!(Yt!==null&&ca.call(Yt,e)))for(var s=0;s<n.length;s++){var o=n[s];o.f&Ot?io(o,t,!1):t===o&&(r?xt(o,Pt):o.f&Nt&&xt(o,nr),fr(o))}}function lo(e){var R;var t=Dt,r=Wt,n=Kt,s=De,o=Yt,l=Ht,d=ar,c=Kr,u=e.f;Dt=null,Wt=0,Kt=null,De=u&(sr|Xr)?null:e,Yt=null,fa(e.ctx),ar=!1,Kr=++Dr,e.ac!==null&&(an(()=>{e.ac.abort(Rr)}),e.ac=null);try{e.f|=kn;var m=e.fn,x=m();e.f|=ha;var w=e.deps,I=_e==null?void 0:_e.is_fork;if(Dt!==null){var T;if(I||Ia(e,Wt),w!==null&&Wt>0)for(w.length=Wt+Dt.length,T=0;T<Dt.length;T++)w[Wt+T]=Dt[T];else e.deps=w=Dt;if(Wn()&&e.f&Jt)for(T=Wt;T<w.length;T++)((R=w[T]).reactions??(R.reactions=[])).push(e)}else!I&&w!==null&&Wt<w.length&&(Ia(e,Wt),w.length=Wt);if(Ps()&&Kt!==null&&!ar&&w!==null&&!(e.f&(Ot|nr|Pt)))for(T=0;T<Kt.length;T++)io(Kt[T],e);if(s!==null&&s!==e){if(Dr++,s.deps!==null)for(let N=0;N<r;N+=1)s.deps[N].rv=Dr;if(t!==null)for(const N of t)N.rv=Dr;Kt!==null&&(n===null?n=Kt:n.push(...Kt))}return e.f&Nr&&(e.f^=Nr),x}catch(N){return Os(N)}finally{e.f^=kn,Dt=t,Wt=r,Kt=n,De=s,Yt=o,fa(l),ar=d,Kr=c}}function Hi(e,t){let r=t.reactions;if(r!==null){var n=Co.call(r,e);if(n!==-1){var s=r.length-1;s===0?r=t.reactions=null:(r[n]=r[s],r.pop())}}if(r===null&&t.f&Ot&&(Dt===null||!ca.call(Dt,t))){var o=t;o.f&Jt&&(o.f^=Jt,o.f&=~Gr),Hn(o),$i(o),Ia(o,0)}}function Ia(e,t){var r=e.deps;if(r!==null)for(var n=t;n<r.length;n++)Hi(e,r[n])}function pa(e){var t=e.f;if(!(t&vr)){xt(e,Nt);var r=Ge,n=Ga;Ge=e,Ga=!0;try{t&(Lr|en)?Di(e):qn(e),Zs(e);var s=lo(e);e.teardown=typeof s=="function"?s:null,e.wv=so;var o;_n&&di&&e.f&Pt&&e.deps}finally{Ga=n,Ge=r}}}async function co(){await Promise.resolve(),vi()}function a(e){var t=e.f,r=(t&Ot)!==0;if(De!==null&&!ar){var n=Ge!==null&&(Ge.f&vr)!==0;if(!n&&(Yt===null||!ca.call(Yt,e))){var s=De.deps;if(De.f&kn)e.rv<Dr&&(e.rv=Dr,Dt===null&&s!==null&&s[Wt]===e?Wt++:Dt===null?Dt=[e]:Dt.push(e));else{(De.deps??(De.deps=[])).push(e);var o=e.reactions;o===null?e.reactions=[De]:ca.call(o,De)||o.push(De)}}}if(Ir&&Or.has(e))return Or.get(e);if(r){var l=e;if(Ir){var d=l.v;return(!(l.f&Nt)&&l.reactions!==null||fo(l))&&(d=Un(l)),Or.set(l,d),d}var c=(l.f&Jt)===0&&!ar&&De!==null&&(Ga||(De.f&Jt)!==0),u=(l.f&ha)===0;Ha(l)&&(c&&(l.f|=Jt),Us(l)),c&&!u&&(Bs(l),uo(l))}if(Mt!=null&&Mt.has(e))return Mt.get(e);if(e.f&Nr)throw e.v;return e.v}function uo(e){if(e.f|=Jt,e.deps!==null)for(const t of e.deps)(t.reactions??(t.reactions=[])).push(e),t.f&Ot&&!(t.f&Jt)&&(Bs(t),uo(t))}function fo(e){if(e.v===$t)return!0;if(e.deps===null)return!1;for(const t of e.deps)if(Or.has(t)||t.f&Ot&&fo(t))return!0;return!1}function xa(e){var t=ar;try{return ar=!0,e()}finally{ar=t}}function Ui(e){return e.endsWith("capture")&&e!=="gotpointercapture"&&e!=="lostpointercapture"}const Bi=["beforeinput","click","change","dblclick","contextmenu","focusin","focusout","input","keydown","keyup","mousedown","mousemove","mouseout","mouseover","mouseup","pointerdown","pointermove","pointerout","pointerover","pointerup","touchend","touchmove","touchstart"];function Wi(e){return Bi.includes(e)}const Vi={formnovalidate:"formNoValidate",ismap:"isMap",nomodule:"noModule",playsinline:"playsInline",readonly:"readOnly",defaultvalue:"defaultValue",defaultchecked:"defaultChecked",srcobject:"srcObject",novalidate:"noValidate",allowfullscreen:"allowFullscreen",disablepictureinpicture:"disablePictureInPicture",disableremoteplayback:"disableRemotePlayback"};function qi(e){return e=e.toLowerCase(),Vi[e]??e}const Ki=["touchstart","touchmove"];function Gi(e){return Ki.includes(e)}const zr=Symbol("events"),vo=new Set,Pn=new Set;function go(e,t,r,n={}){function s(o){if(n.capture||Tn.call(t,o),!o.cancelBubble)return an(()=>r==null?void 0:r.call(this,o))}return e.startsWith("pointer")||e.startsWith("touch")||e==="wheel"?gr(()=>{t.addEventListener(e,s,n)}):t.addEventListener(e,s,n),s}function Fr(e,t,r,n,s){var o={capture:n,passive:s},l=go(e,t,r,o);(t===document.body||t===window||t===document||t instanceof HTMLMediaElement)&&nn(()=>{t.removeEventListener(e,l,o)})}function J(e,t,r){(t[zr]??(t[zr]={}))[e]=r}function or(e){for(var t=0;t<e.length;t++)vo.add(e[t]);for(var r of Pn)r(e)}let ls=null;function Tn(e){var N,O;var t=this,r=t.ownerDocument,n=e.type,s=((N=e.composedPath)==null?void 0:N.call(e))||[],o=s[0]||e.target;ls=e;var l=0,d=ls===e&&e[zr];if(d){var c=s.indexOf(d);if(c!==-1&&(t===document||t===window)){e[zr]=t;return}var u=s.indexOf(t);if(u===-1)return;c<=u&&(l=c)}if(o=s[l]||e.target,o!==t){Mo(e,"currentTarget",{configurable:!0,get(){return o||r}});var m=De,x=Ge;Xt(null),pr(null);try{for(var w,I=[];o!==null;){var T=o.assignedSlot||o.parentNode||o.host||null;try{var R=(O=o[zr])==null?void 0:O[n];R!=null&&(!o.disabled||e.target===o)&&R.call(o,e)}catch(Z){w?I.push(Z):w=Z}if(e.cancelBubble||T===t||T===null)break;o=T}if(w){for(let Z of I)queueMicrotask(()=>{throw Z});throw w}}finally{e[zr]=t,delete e.currentTarget,Xt(m),pr(x)}}}var _s;const fn=((_s=globalThis==null?void 0:globalThis.window)==null?void 0:_s.trustedTypes)&&globalThis.window.trustedTypes.createPolicy("svelte-trusted-html",{createHTML:e=>e});function Ji(e){return(fn==null?void 0:fn.createHTML(e))??e}function po(e){var t=Bn("template");return t.innerHTML=Ji(e.replaceAll("<!>","<!---->")),t.content}function ba(e,t){var r=Ge;r.nodes===null&&(r.nodes={start:e,end:t,a:null,t:null})}function k(e,t){var r=(t&ri)!==0,n=(t&ai)!==0,s,o=!e.startsWith("<!>");return()=>{s===void 0&&(s=po(o?e:"<!>"+e),r||(s=xr(s)));var l=n||qs?document.importNode(s,!0):s.cloneNode(!0);if(r){var d=xr(l),c=l.lastChild;ba(d,c)}else ba(l,l);return l}}function Yi(e,t,r="svg"){var n=!e.startsWith("<!>"),s=`<${r}>${n?e:"<!>"+e}</${r}>`,o;return()=>{if(!o){var l=po(s),d=xr(l);o=xr(d)}var c=o.cloneNode(!0);return ba(c,c),c}}function Xi(e,t){return Yi(e,t,"svg")}function Re(){var e=document.createDocumentFragment(),t=document.createComment(""),r=_r();return e.append(t,r),ba(t,r),e}function v(e,t){e!==null&&e.before(t)}function p(e,t){var r=t==null?"":typeof t=="object"?`${t}`:t;r!==(e.__t??(e.__t=e.nodeValue))&&(e.__t=r,e.nodeValue=`${r}`)}function Qi(e,t){return Zi(e,t)}const Wa=new Map;function Zi(e,{target:t,anchor:r,props:n={},events:s,context:o,intro:l=!0,transformError:d}){Pi();var c=void 0,u=Ri(()=>{var m=r??t.appendChild(_r());yi(m,{pending:()=>{}},I=>{$e({});var T=Ht;o&&(T.c=o),s&&(n.$$events=s),c=e(I,n)||{},Ce()},d);var x=new Set,w=I=>{for(var T=0;T<I.length;T++){var R=I[T];if(!x.has(R)){x.add(R);var N=Gi(R);for(const q of[t,document]){var O=Wa.get(q);O===void 0&&(O=new Map,Wa.set(q,O));var Z=O.get(R);Z===void 0?(q.addEventListener(R,Tn,{passive:N}),O.set(R,1)):O.set(R,Z+1)}}}};return w(Za(vo)),Pn.add(w),()=>{var N;for(var I of x)for(const O of[t,document]){var T=Wa.get(O),R=T.get(I);--R==0?(O.removeEventListener(I,Tn),T.delete(I),T.size===0&&Wa.delete(O)):T.set(I,R)}Pn.delete(w),m!==r&&((N=m.parentNode)==null||N.removeChild(m))}});return el.set(c,u),c}let el=new WeakMap;var rr,cr,qt,Vr,ja,Da,Qa;class on{constructor(t,r=!0){Zt(this,"anchor");Be(this,rr,new Map);Be(this,cr,new Map);Be(this,qt,new Map);Be(this,Vr,new Set);Be(this,ja,!0);Be(this,Da,t=>{if($(this,rr).has(t)){var r=$(this,rr).get(t),n=$(this,cr).get(r);if(n)Kn(n),$(this,Vr).delete(r);else{var s=$(this,qt).get(r);s&&!(s.effect.f&It)&&($(this,cr).set(r,s.effect),$(this,qt).delete(r),s.fragment.lastChild.remove(),this.anchor.before(s.fragment),n=s.effect)}for(const[o,l]of $(this,rr)){if($(this,rr).delete(o),o===t)break;const d=$(this,qt).get(l);d&&(Tt(d.effect),$(this,qt).delete(l))}for(const[o,l]of $(this,cr)){if(o===r||$(this,Vr).has(o)||l.f&It)continue;const d=()=>{if(Array.from($(this,rr).values()).includes(o)){var u=document.createDocumentFragment();Gn(l,u),u.append(_r()),$(this,qt).set(o,{effect:l,fragment:u})}else Tt(l);$(this,Vr).delete(o),$(this,cr).delete(o)};$(this,ja)||!n?($(this,Vr).add(o),qr(l,d,!1)):d()}}});Be(this,Qa,t=>{$(this,rr).delete(t);const r=Array.from($(this,rr).values());for(const[n,s]of $(this,qt))r.includes(n)||(Tt(s.effect),$(this,qt).delete(n))});this.anchor=t,Oe(this,ja,r)}ensure(t,r){var n=_e,s=Js();if(r&&!$(this,cr).has(t)&&!$(this,qt).has(t))if(s){var o=document.createDocumentFragment(),l=_r();o.append(l),$(this,qt).set(t,{effect:zt(()=>r(l)),fragment:o})}else $(this,cr).set(t,zt(()=>r(this.anchor)));if($(this,rr).set(n,t),s){for(const[d,c]of $(this,cr))d===t?n.unskip_effect(c):n.skip_effect(c);for(const[d,c]of $(this,qt))d===t?n.unskip_effect(c.effect):n.skip_effect(c.effect);n.oncommit($(this,Da)),n.ondiscard($(this,Qa))}else $(this,Da).call(this,n)}}rr=new WeakMap,cr=new WeakMap,qt=new WeakMap,Vr=new WeakMap,ja=new WeakMap,Da=new WeakMap,Qa=new WeakMap;function K(e,t,r=!1){var n=new on(e),s=r?kr:0;function o(l,d){n.ensure(l,d)}_a(()=>{var l=!1;t((d,c=0)=>{l=!0,o(c,d)}),l||o(-1,null)},s)}function lt(e,t){return t}function tl(e,t,r){for(var n=[],s=t.length,o,l=t.length,d=0;d<s;d++){let x=t[d];qr(x,()=>{if(o){if(o.pending.delete(x),o.done.add(x),o.pending.size===0){var w=e.outrogroups;On(e,Za(o.done)),w.delete(o),w.size===0&&(e.outrogroups=null)}}else l-=1},!1)}if(l===0){var c=n.length===0&&r!==null;if(c){var u=r,m=u.parentNode;Ti(m),m.append(u),e.items.clear()}On(e,t,!c)}else o={pending:new Set(t),done:new Set},(e.outrogroups??(e.outrogroups=new Set)).add(o)}function On(e,t,r=!0){var n;if(e.pending.size>0){n=new Set;for(const l of e.pending.values())for(const d of l)n.add(e.items.get(d).e)}for(var s=0;s<t.length;s++){var o=t[s];if(n!=null&&n.has(o)){o.f|=ur;const l=document.createDocumentFragment();Gn(o,l)}else Tt(t[s],r)}}var ds;function at(e,t,r,n,s,o=null){var l=e,d=new Map,c=(t&Es)!==0;if(c){var u=e;l=u.appendChild(_r())}var m=null,x=Hs(()=>{var q=r();return jn(q)?q:q==null?[]:Za(q)}),w,I=new Map,T=!0;function R(q){Z.effect.f&vr||(Z.pending.delete(q),Z.fallback=m,rl(Z,w,l,t,n),m!==null&&(w.length===0?m.f&ur?(m.f^=ur,Na(m,null,l)):Kn(m):qr(m,()=>{m=null})))}function N(q){Z.pending.delete(q)}var O=_a(()=>{w=a(x);for(var q=w.length,E=new Set,h=_e,M=Js(),C=0;C<q;C+=1){var U=w[C],ie=n(U,C),fe=T?null:d.get(ie);fe?(fe.v&&ga(fe.v,U),fe.i&&ga(fe.i,C),M&&h.unskip_effect(fe.e)):(fe=al(d,T?l:ds??(ds=_r()),U,ie,C,s,t,r),T||(fe.e.f|=ur),d.set(ie,fe)),E.add(ie)}if(q===0&&o&&!m&&(T?m=zt(()=>o(l)):(m=zt(()=>o(ds??(ds=_r()))),m.f|=ur)),q>E.size&&jo(),!T)if(I.set(h,E),M){for(const[We,Me]of d)E.has(We)||h.skip_effect(Me.e);h.oncommit(R),h.ondiscard(N)}else R(h);a(x)}),Z={effect:O,items:d,pending:I,outrogroups:null,fallback:m};T=!1}function Ea(e){for(;e!==null&&!(e.f&sr);)e=e.next;return e}function rl(e,t,r,n,s){var fe,We,Me,W,Y,be,ae,ze,B;var o=(n&Yo)!==0,l=t.length,d=e.items,c=Ea(e.effect.first),u,m=null,x,w=[],I=[],T,R,N,O;if(o)for(O=0;O<l;O+=1)T=t[O],R=s(T,O),N=d.get(R).e,N.f&ur||((We=(fe=N.nodes)==null?void 0:fe.a)==null||We.measure(),(x??(x=new Set)).add(N));for(O=0;O<l;O+=1){if(T=t[O],R=s(T,O),N=d.get(R).e,e.outrogroups!==null)for(const V of e.outrogroups)V.pending.delete(N),V.done.delete(N);if(N.f&ur)if(N.f^=ur,N===c)Na(N,null,r);else{var Z=m?m.next:c;N===e.effect.last&&(e.effect.last=N.prev),N.prev&&(N.prev.next=N.next),N.next&&(N.next.prev=N.prev),wr(e,m,N),wr(e,N,Z),Na(N,Z,r),m=N,w=[],I=[],c=Ea(m.next);continue}if(N.f&It&&(Kn(N),o&&((W=(Me=N.nodes)==null?void 0:Me.a)==null||W.unfix(),(x??(x=new Set)).delete(N))),N!==c){if(u!==void 0&&u.has(N)){if(w.length<I.length){var q=I[0],E;m=q.prev;var h=w[0],M=w[w.length-1];for(E=0;E<w.length;E+=1)Na(w[E],q,r);for(E=0;E<I.length;E+=1)u.delete(I[E]);wr(e,h.prev,M.next),wr(e,m,h),wr(e,M,q),c=q,m=M,O-=1,w=[],I=[]}else u.delete(N),Na(N,c,r),wr(e,N.prev,N.next),wr(e,N,m===null?e.effect.first:m.next),wr(e,m,N),m=N;continue}for(w=[],I=[];c!==null&&c!==N;)(u??(u=new Set)).add(c),I.push(c),c=Ea(c.next);if(c===null)continue}N.f&ur||w.push(N),m=N,c=Ea(N.next)}if(e.outrogroups!==null){for(const V of e.outrogroups)V.pending.size===0&&(On(e,Za(V.done)),(Y=e.outrogroups)==null||Y.delete(V));e.outrogroups.size===0&&(e.outrogroups=null)}if(c!==null||u!==void 0){var C=[];if(u!==void 0)for(N of u)N.f&It||C.push(N);for(;c!==null;)!(c.f&It)&&c!==e.fallback&&C.push(c),c=Ea(c.next);var U=C.length;if(U>0){var ie=n&Es&&l===0?r:null;if(o){for(O=0;O<U;O+=1)(ae=(be=C[O].nodes)==null?void 0:be.a)==null||ae.measure();for(O=0;O<U;O+=1)(B=(ze=C[O].nodes)==null?void 0:ze.a)==null||B.fix()}tl(e,C,ie)}}o&&gr(()=>{var V,ve;if(x!==void 0)for(N of x)(ve=(V=N.nodes)==null?void 0:V.a)==null||ve.apply()})}function al(e,t,r,n,s,o,l,d){var c=l&Go?l&Xo?Jr(r):Ci(r,!1,!1):null,u=l&Jo?Jr(s):null;return{v:c,i:u,e:zt(()=>(o(t,c??r,u??s,d),()=>{e.delete(n)}))}}function Na(e,t,r){if(e.nodes)for(var n=e.nodes.start,s=e.nodes.end,o=t&&!(t.f&ur)?t.nodes.start:r;n!==null;){var l=za(n);if(o.before(n),n===s)return;n=l}}function wr(e,t,r){t===null?e.effect.first=r:t.next=r,r===null?e.effect.last=t:r.prev=t}function nl(e,t,r=!1,n=!1,s=!1){var o=e,l="";P(()=>{var d=Ge;if(l!==(l=t()??"")&&(d.nodes!==null&&(eo(d.nodes.start,d.nodes.end),d.nodes=null),l!=="")){var c=r?Cs:n?ni:void 0,u=Bn(r?"svg":n?"math":"template",c);u.innerHTML=l;var m=r||n?u:u.content;if(ba(xr(m),m.lastChild),r||n)for(;xr(m);)o.before(xr(m));else o.before(m)}})}function ft(e,t,...r){var n=new on(e);_a(()=>{const s=t()??null;n.ensure(s,s&&(o=>s(o,...r)))},kr)}function sl(e,t,r){var n=new on(e);_a(()=>{var s=t()??null;n.ensure(s,s&&(o=>r(o,s)))},kr)}function ol(e,t,r,n,s,o){var l=null,d=e,c=new on(d,!1);_a(()=>{const u=t()||null;var m=Cs;if(u===null){c.ensure(null,null);return}return c.ensure(u,x=>{if(u){if(l=Bn(u,m),ba(l,l),n){var w=l.appendChild(_r());n(l,w)}Ge.nodes.end=l,x.before(l)}}),()=>{}},kr),nn(()=>{})}function il(e,t){var r=void 0,n;Qs(()=>{r!==(r=t())&&(n&&(Tt(n),n=null),r&&(n=zt(()=>{sn(()=>r(e))})))})}function bo(e){var t,r,n="";if(typeof e=="string"||typeof e=="number")n+=e;else if(typeof e=="object")if(Array.isArray(e)){var s=e.length;for(t=0;t<s;t++)e[t]&&(r=bo(e[t]))&&(n&&(n+=" "),n+=r)}else for(r in e)e[r]&&(n&&(n+=" "),n+=r);return n}function ll(){for(var e,t,r=0,n="",s=arguments.length;r<s;r++)(e=arguments[r])&&(t=bo(e))&&(n&&(n+=" "),n+=t);return n}function yo(e){return typeof e=="object"?ll(e):e??""}const cs=[...` 	
\r\f \v\uFEFF`];function dl(e,t,r){var n=e==null?"":""+e;if(r){for(var s of Object.keys(r))if(r[s])n=n?n+" "+s:s;else if(n.length)for(var o=s.length,l=0;(l=n.indexOf(s,l))>=0;){var d=l+o;(l===0||cs.includes(n[l-1]))&&(d===n.length||cs.includes(n[d]))?n=(l===0?"":n.substring(0,l))+n.substring(d+1):l=d}}return n===""?null:n}function us(e,t=!1){var r=t?" !important;":";",n="";for(var s of Object.keys(e)){var o=e[s];o!=null&&o!==""&&(n+=" "+s+": "+o+r)}return n}function vn(e){return e[0]!=="-"||e[1]!=="-"?e.toLowerCase():e}function cl(e,t){if(t){var r="",n,s;if(Array.isArray(t)?(n=t[0],s=t[1]):n=t,e){e=String(e).replaceAll(/\s*\/\*.*?\*\/\s*/g,"").trim();var o=!1,l=0,d=!1,c=[];n&&c.push(...Object.keys(n).map(vn)),s&&c.push(...Object.keys(s).map(vn));var u=0,m=-1;const R=e.length;for(var x=0;x<R;x++){var w=e[x];if(d?w==="/"&&e[x-1]==="*"&&(d=!1):o?o===w&&(o=!1):w==="/"&&e[x+1]==="*"?d=!0:w==='"'||w==="'"?o=w:w==="("?l++:w===")"&&l--,!d&&o===!1&&l===0){if(w===":"&&m===-1)m=x;else if(w===";"||x===R-1){if(m!==-1){var I=vn(e.substring(u,m).trim());if(!c.includes(I)){w!==";"&&x++;var T=e.substring(u,x).trim();r+=" "+T+";"}}u=x+1,m=-1}}}}return n&&(r+=us(n)),s&&(r+=us(s,!0)),r=r.trim(),r===""?null:r}return e==null?null:String(e)}function tt(e,t,r,n,s,o){var l=e.__className;if(l!==r||l===void 0){var d=dl(r,n,o);d==null?e.removeAttribute("class"):t?e.className=d:e.setAttribute("class",d),e.__className=r}else if(o&&s!==o)for(var c in o){var u=!!o[c];(s==null||u!==!!s[c])&&e.classList.toggle(c,u)}return o}function gn(e,t={},r,n){for(var s in r){var o=r[s];t[s]!==o&&(r[s]==null?e.style.removeProperty(s):e.style.setProperty(s,o,n))}}function ul(e,t,r,n){var s=e.__style;if(s!==t){var o=cl(t,n);o==null?e.removeAttribute("style"):e.style.cssText=o,e.__style=t}else n&&(Array.isArray(n)?(gn(e,r==null?void 0:r[0],n[0]),gn(e,r==null?void 0:r[1],n[1],"important")):gn(e,r,n));return n}function La(e,t,r=!1){if(e.multiple){if(t==null)return;if(!jn(t))return oi();for(var n of e.options)n.selected=t.includes(Oa(n));return}for(n of e.options){var s=Oa(n);if(Ni(s,t)){n.selected=!0;return}}(!r||t!==void 0)&&(e.selectedIndex=-1)}function Jn(e){var t=new MutationObserver(()=>{La(e,e.__value)});t.observe(e,{childList:!0,subtree:!0,attributes:!0,attributeFilter:["value"]}),nn(()=>{t.disconnect()})}function In(e,t,r=t){var n=new WeakSet,s=!0;Ys(e,"change",o=>{var l=o?"[selected]":":checked",d;if(e.multiple)d=[].map.call(e.querySelectorAll(l),Oa);else{var c=e.querySelector(l)??e.querySelector("option:not([disabled])");d=c&&Oa(c)}r(d),_e!==null&&n.add(_e)}),sn(()=>{var o=t();if(e===document.activeElement){var l=Ja??_e;if(n.has(l))return}if(La(e,o,s),s&&o===void 0){var d=e.querySelector(":checked");d!==null&&(o=Oa(d),r(o))}e.__value=o,s=!1}),Jn(e)}function Oa(e){return"__value"in e?e.__value:e.value}const $a=Symbol("class"),Ca=Symbol("style"),ho=Symbol("is custom element"),mo=Symbol("is html"),fl=zn?"option":"OPTION",vl=zn?"select":"SELECT",gl=zn?"progress":"PROGRESS";function yr(e,t){var r=Yn(e);r.value===(r.value=t??void 0)||e.value===t&&(t!==0||e.nodeName!==gl)||(e.value=t??"")}function pl(e,t){t?e.hasAttribute("selected")||e.setAttribute("selected",""):e.removeAttribute("selected")}function ht(e,t,r,n){var s=Yn(e);s[t]!==(s[t]=r)&&(t==="loading"&&(e[Fo]=r),r==null?e.removeAttribute(t):typeof r!="string"&&_o(e).includes(t)?e[t]=r:e.setAttribute(t,r))}function bl(e,t,r,n,s=!1,o=!1){var l=Yn(e),d=l[ho],c=!l[mo],u=t||{},m=e.nodeName===fl;for(var x in t)x in r||(r[x]=null);r.class?r.class=yo(r.class):r[$a]&&(r.class=null),r[Ca]&&(r.style??(r.style=null));var w=_o(e);for(const E in r){let h=r[E];if(m&&E==="value"&&h==null){e.value=e.__value="",u[E]=h;continue}if(E==="class"){var I=e.namespaceURI==="http://www.w3.org/1999/xhtml";tt(e,I,h,n,t==null?void 0:t[$a],r[$a]),u[E]=h,u[$a]=r[$a];continue}if(E==="style"){ul(e,h,t==null?void 0:t[Ca],r[Ca]),u[E]=h,u[Ca]=r[Ca];continue}var T=u[E];if(!(h===T&&!(h===void 0&&e.hasAttribute(E)))){u[E]=h;var R=E[0]+E[1];if(R!=="$$")if(R==="on"){const M={},C="$$"+E;let U=E.slice(2);var N=Wi(U);if(Ui(U)&&(U=U.slice(0,-7),M.capture=!0),!N&&T){if(h!=null)continue;e.removeEventListener(U,u[C],M),u[C]=null}if(N)J(U,e,h),or([U]);else if(h!=null){let ie=function(fe){u[E].call(this,fe)};var q=ie;u[C]=go(U,e,ie,M)}}else if(E==="style")ht(e,E,h);else if(E==="autofocus")Oi(e,!!h);else if(!d&&(E==="__value"||E==="value"&&h!=null))e.value=e.__value=h;else if(E==="selected"&&m)pl(e,h);else{var O=E;c||(O=qi(O));var Z=O==="defaultValue"||O==="defaultChecked";if(h==null&&!d&&!Z)if(l[E]=null,O==="value"||O==="checked"){let M=e;const C=t===void 0;if(O==="value"){let U=M.defaultValue;M.removeAttribute(O),M.defaultValue=U,M.value=M.__value=C?U:null}else{let U=M.defaultChecked;M.removeAttribute(O),M.defaultChecked=U,M.checked=C?U:!1}}else e.removeAttribute(E);else Z||w.includes(O)&&(d||typeof h!="string")?(e[O]=h,O in l&&(l[O]=$t)):typeof h!="function"&&ht(e,O,h)}}}return u}function fs(e,t,r=[],n=[],s=[],o,l=!1,d=!1){zs(s,r,n,c=>{var u=void 0,m={},x=e.nodeName===vl,w=!1;if(Qs(()=>{var T=t(...c.map(a)),R=bl(e,u,T,o,l,d);w&&x&&"value"in T&&La(e,T.value);for(let O of Object.getOwnPropertySymbols(m))T[O]||Tt(m[O]);for(let O of Object.getOwnPropertySymbols(T)){var N=T[O];O.description===si&&(!u||N!==u[O])&&(m[O]&&Tt(m[O]),m[O]=zt(()=>il(e,()=>N))),R[O]=N}u=R}),x){var I=e;sn(()=>{La(I,u.value,!0),Jn(I)})}w=!0})}function Yn(e){return e.__attributes??(e.__attributes={[ho]:e.nodeName.includes("-"),[mo]:e.namespaceURI===$s})}var vs=new Map;function _o(e){var t=e.getAttribute("is")||e.nodeName,r=vs.get(t);if(r)return r;vs.set(t,r=[]);for(var n,s=e,o=Element.prototype;o!==s;){n=No(s);for(var l in n)n[l].set&&r.push(l);s=ks(s)}return r}function Hr(e,t,r=t){var n=new WeakSet;Ys(e,"input",async s=>{var o=s?e.defaultValue:e.value;if(o=pn(e)?bn(o):o,r(o),_e!==null&&n.add(_e),await co(),o!==(o=t())){var l=e.selectionStart,d=e.selectionEnd,c=e.value.length;if(e.value=o??"",d!==null){var u=e.value.length;l===d&&d===c&&u>c?(e.selectionStart=u,e.selectionEnd=u):(e.selectionStart=l,e.selectionEnd=Math.min(d,u))}}}),xa(t)==null&&e.value&&(r(pn(e)?bn(e.value):e.value),_e!==null&&n.add(_e)),Vn(()=>{var s=t();if(e===document.activeElement){var o=Ja??_e;if(n.has(o))return}pn(e)&&s===bn(e.value)||e.type==="date"&&!s&&!e.value||s!==e.value&&(e.value=s??"")})}function pn(e){var t=e.type;return t==="number"||t==="range"}function bn(e){return e===""?null:+e}function gs(e,t){return e===t||(e==null?void 0:e[Pr])===t}function Ln(e={},t,r,n){return sn(()=>{var s,o;return Vn(()=>{s=o,o=[],xa(()=>{e!==r(...o)&&(t(e,...o),s&&gs(r(...s),e)&&t(null,...s))})}),()=>{gr(()=>{o&&gs(r(...o),e)&&t(null,...o)})}}),e}let Va=!1;function yl(e){var t=Va;try{return Va=!1,[e(),Va]}finally{Va=t}}const hl={get(e,t){if(!e.exclude.includes(t))return e.props[t]},set(e,t){return!1},getOwnPropertyDescriptor(e,t){if(!e.exclude.includes(t)&&t in e.props)return{enumerable:!0,configurable:!0,value:e.props[t]}},has(e,t){return e.exclude.includes(t)?!1:t in e.props},ownKeys(e){return Reflect.ownKeys(e.props).filter(t=>!e.exclude.includes(t))}};function vt(e,t,r){return new Proxy({props:e,exclude:t},hl)}const ml={get(e,t){let r=e.props.length;for(;r--;){let n=e.props[r];if(Aa(n)&&(n=n()),typeof n=="object"&&n!==null&&t in n)return n[t]}},set(e,t,r){let n=e.props.length;for(;n--;){let s=e.props[n];Aa(s)&&(s=s());const o=Mr(s,t);if(o&&o.set)return o.set(r),!0}return!1},getOwnPropertyDescriptor(e,t){let r=e.props.length;for(;r--;){let n=e.props[r];if(Aa(n)&&(n=n()),typeof n=="object"&&n!==null&&t in n){const s=Mr(n,t);return s&&!s.configurable&&(s.configurable=!0),s}}},has(e,t){if(t===Pr||t===Ss)return!1;for(let r of e.props)if(Aa(r)&&(r=r()),r!=null&&t in r)return!0;return!1},ownKeys(e){const t=[];for(let r of e.props)if(Aa(r)&&(r=r()),!!r){for(const n in r)t.includes(n)||t.push(n);for(const n of Object.getOwnPropertySymbols(r))t.includes(n)||t.push(n)}return t}};function gt(...e){return new Proxy({props:e},ml)}function ea(e,t,r,n){var Z;var s=(r&ei)!==0,o=(r&ti)!==0,l=n,d=!0,c=()=>(d&&(d=!1,l=o?xa(n):n),l),u;if(s){var m=Pr in e||Ss in e;u=((Z=Mr(e,t))==null?void 0:Z.set)??(m&&t in e?q=>e[t]=q:void 0)}var x,w=!1;s?[x,w]=yl(()=>e[t]):x=e[t],x===void 0&&n!==void 0&&(x=c(),u&&(Bo(),u(x)));var I;if(I=()=>{var q=e[t];return q===void 0?c():(d=!0,q)},!(r&Zo))return I;if(u){var T=e.$$legacy;return function(q,E){return arguments.length>0?((!E||T||w)&&u(E?I():q),q):I()}}var R=!1,N=(r&Qo?rn:Hs)(()=>(R=!1,I()));s&&a(N);var O=Ge;return function(q,E){if(arguments.length>0){const h=E?a(N):s?kt(q):q;return y(N,h),R=!0,l!==void 0&&(l=h),q}return Ir&&R||O.f&vr?N.v:a(N)}}function _l(e){Ht===null&&As(),Ut(()=>{const t=xa(e);if(typeof t=="function")return t})}function xl(e){Ht===null&&As(),_l(()=>()=>xa(e))}const kl="5";var xs;typeof window<"u"&&((xs=window.__svelte??(window.__svelte={})).v??(xs.v=new Set)).add(kl);const Xn="prx-console-token",wl=[{labelKey:"nav.overview",path:"/overview"},{labelKey:"nav.sessions",path:"/sessions"},{labelKey:"nav.channels",path:"/channels"},{labelKey:"nav.hooks",path:"/hooks"},{labelKey:"nav.mcp",path:"/mcp"},{labelKey:"nav.skills",path:"/skills"},{labelKey:"nav.plugins",path:"/plugins"},{labelKey:"nav.config",path:"/config"},{labelKey:"nav.logs",path:"/logs"}];function Fa(){var e;return typeof window>"u"?"":((e=window.localStorage.getItem(Xn))==null?void 0:e.trim())??""}function Sl(e){typeof window>"u"||window.localStorage.setItem(Xn,e.trim())}function xo(){typeof window>"u"||window.localStorage.removeItem(Xn)}const Al={title:"PRX Console",menu:"Menu",closeSidebar:"Close sidebar",language:"Language",notFound:"Not found",backToOverview:"Back to Overview"},El={overview:"Overview",sessions:"Sessions",channels:"Channels",config:"Config",hooks:"Hooks",mcp:"MCP",skills:"Skills",plugins:"Plugins",logs:"Logs"},$l={logout:"Logout",loading:"Loading...",error:"Error",refresh:"Refresh",updatedAt:"Updated {time}",na:"N/A",enabled:"Enabled",disabled:"Disabled",yes:"Yes",no:"No",unknown:"Unknown",clipboardUnavailable:"Clipboard not available.",copied:"Copied",copyFailed:"Copy failed",empty:"Empty"},Cl={title:"Overview",version:"Version",uptime:"Uptime",model:"Model",memoryBackend:"Memory Backend",gatewayPort:"Gateway Port",configuredChannels:"Configured Channels",loading:"Loading status...",loadFailed:"Failed to load status.",noChannelsConfigured:"No channels configured."},Ml={title:"Sessions",sessionId:"Session ID",sender:"Sender",channel:"Channel",messages:"Messages",lastMessage:"Last Message",loading:"Loading sessions...",loadFailed:"Failed to load sessions.",none:"No active sessions"},Nl={title:"Chat",session:"Session",back:"Back to Sessions",loading:"Loading messages...",loadFailed:"Failed to load messages.",sendFailed:"Failed to send message.",empty:"No messages in this session.",inputPlaceholder:"Type a message...",send:"Send",sending:"Sending..."},Pl={title:"Channels",type:"Type",loading:"Loading channels...",loadFailed:"Failed to load channel status.",noChannels:"No channels available.",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"Email",irc:"IRC",lark:"Lark",dingtalk:"DingTalk",qq:"QQ",cli:"CLI"}},Tl={title:"Config",rawJson:"Raw JSON",structured:"Structured View",copy:"Copy",copyJson:"Copy JSON",loading:"Loading config...",loadFailed:"Failed to load config.",section:{general:"General",gateway:"Gateway",channels:"Channels",memory:"Memory",security:"Security",model:"Model",other:"Other"},field:{version:"Version",runtimeModel:"Runtime Model",memoryBackend:"Memory Backend",configuredChannels:"Configured Channels",notConfigured:"Not configured",notSet:"Not set"},channel:{settings:"settings",notConfigured:"Not configured"},redacted:"Redacted",emptyObject:"No settings"},Ol={title:"Logs",connected:"Connected",disconnected:"Disconnected",reconnecting:"Reconnecting",pause:"Pause",resume:"Resume",clear:"Clear",waiting:"Waiting for log stream..."},Il={title:"Hooks",loading:"Loading hooks...",noHooks:"No hooks configured.",addHook:"Add Hook",cancelAdd:"Cancel",newHook:"New Hook",event:"Event",command:"Command",commandPlaceholder:"e.g. /opt/scripts/on-event.sh",timeout:"Timeout (ms)",enabled:"Enabled",edit:"Edit",delete:"Delete",save:"Save",cancel:"Cancel"},Ll={title:"MCP Servers",loading:"Loading MCP servers...",noServers:"No MCP servers configured.",connected:"Connected",disconnected:"Disconnected",tools:"tools",availableTools:"Available Tools",noTools:"No tools available."},Fl={title:"Skills",loading:"Loading skills...",noSkills:"No skills installed.",active:"active",tabInstalled:"Installed",tabDiscover:"Discover",search:"Search skills...",source:"Source",searchBtn:"Search",searching:"Searching...",noResults:"No results found.",install:"Install",installing:"Installing...",installed:"Installed",uninstall:"Uninstall",uninstalling:"Removing...",confirmUninstall:'Are you sure you want to uninstall "{name}"?',stars:"stars",owner:"by",licensed:"Licensed",unlicensed:"No license",installSuccess:"Skill installed successfully",installFailed:"Failed to install skill",uninstallSuccess:"Skill uninstalled",uninstallFailed:"Failed to uninstall skill"},Rl={title:"Plugins",loading:"Loading plugins...",loadFailed:"Failed to load plugins.",noPlugins:"No WASM plugins loaded.",capabilities:"Capabilities",permissions:"Permissions",statusActive:"Active",reload:"Reload",reloadSuccess:'Plugin "{name}" reloaded',reloadFailed:"Failed to reload plugin"},jl={title:"PRX Console Login",accessToken:"Access Token",login:"Login",hint:"Enter your gateway auth token to continue.",placeholder:"Bearer token",tokenRequired:"Access token is required."},Dl={app:Al,nav:El,common:$l,overview:Cl,sessions:Ml,chat:Nl,channels:Pl,config:Tl,logs:Ol,hooks:Il,mcp:Ll,skills:Fl,plugins:Rl,login:jl},zl={title:"PRX 控制台",menu:"菜单",closeSidebar:"关闭侧边栏",language:"语言",notFound:"页面未找到",backToOverview:"返回概览"},Hl={overview:"概览",sessions:"会话",channels:"通道",config:"配置",hooks:"Hooks",mcp:"MCP",skills:"Skills",plugins:"插件",logs:"日志"},Ul={logout:"退出登录",loading:"加载中...",error:"错误",refresh:"刷新",updatedAt:"更新时间 {time}",na:"暂无",enabled:"已启用",disabled:"已禁用",yes:"是",no:"否",unknown:"未知",clipboardUnavailable:"当前环境不支持剪贴板。",copied:"已复制",copyFailed:"复制失败",empty:"空"},Bl={title:"概览",version:"版本",uptime:"运行时长",model:"模型",memoryBackend:"记忆后端",gatewayPort:"网关端口",configuredChannels:"已配置通道",loading:"正在加载状态...",loadFailed:"加载状态失败。",noChannelsConfigured:"尚未配置任何通道。"},Wl={title:"会话",sessionId:"会话 ID",sender:"发送方",channel:"通道",messages:"消息数",lastMessage:"最后消息",loading:"正在加载会话...",loadFailed:"加载会话失败。",none:"当前没有活跃会话"},Vl={title:"聊天",session:"会话",back:"返回会话列表",loading:"正在加载消息...",loadFailed:"加载消息失败。",sendFailed:"发送消息失败。",empty:"此会话暂无消息。",inputPlaceholder:"输入消息...",send:"发送",sending:"发送中..."},ql={title:"通道",type:"类型",loading:"正在加载通道状态...",loadFailed:"加载通道状态失败。",noChannels:"暂无通道数据。",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"邮件",irc:"IRC",lark:"飞书",dingtalk:"钉钉",qq:"QQ",cli:"命令行"}},Kl={title:"配置",rawJson:"原始 JSON",structured:"结构化视图",copy:"复制",copyJson:"复制 JSON",loading:"正在加载配置...",loadFailed:"加载配置失败。",section:{general:"常规",gateway:"网关",channels:"通道",memory:"记忆",security:"安全",model:"模型",other:"其他"},field:{version:"版本",runtimeModel:"运行模型",memoryBackend:"记忆后端",configuredChannels:"已配置通道",notConfigured:"未配置",notSet:"未设置"},channel:{settings:"配置",notConfigured:"未配置"},redacted:"已脱敏",emptyObject:"无配置项"},Gl={title:"日志",connected:"已连接",disconnected:"已断开",reconnecting:"重连中",pause:"暂停",resume:"继续",clear:"清空",waiting:"等待日志流..."},Jl={title:"Hooks",loading:"正在加载 Hooks...",noHooks:"尚未配置任何 Hook。",addHook:"添加 Hook",cancelAdd:"取消",newHook:"新建 Hook",event:"事件",command:"命令",commandPlaceholder:"例如 /opt/scripts/on-event.sh",timeout:"超时 (ms)",enabled:"启用",edit:"编辑",delete:"删除",save:"保存",cancel:"取消"},Yl={title:"MCP 服务",loading:"正在加载 MCP 服务...",noServers:"尚未配置任何 MCP 服务。",connected:"已连接",disconnected:"已断开",tools:"个工具",availableTools:"可用工具",noTools:"无可用工具。"},Xl={title:"Skills",loading:"正在加载 Skills...",noSkills:"尚未安装任何 Skill。",active:"已启用",tabInstalled:"已安装",tabDiscover:"发现新 Skills",search:"搜索 Skills...",source:"来源",searchBtn:"搜索",searching:"搜索中...",noResults:"未找到结果。",install:"安装",installing:"安装中...",installed:"已安装",uninstall:"卸载",uninstalling:"卸载中...",confirmUninstall:'确定要卸载 "{name}" 吗？',stars:"星标",owner:"作者",licensed:"有许可证",unlicensed:"无许可证",installSuccess:"Skill 安装成功",installFailed:"Skill 安装失败",uninstallSuccess:"Skill 已卸载",uninstallFailed:"Skill 卸载失败"},Ql={title:"插件",loading:"正在加载插件...",loadFailed:"加载插件失败。",noPlugins:"未加载任何 WASM 插件。",capabilities:"能力",permissions:"权限",statusActive:"运行中",reload:"重载",reloadSuccess:'插件 "{name}" 已重载',reloadFailed:"插件重载失败"},Zl={title:"PRX 控制台登录",accessToken:"访问令牌",login:"登录",hint:"请输入网关认证令牌以继续。",placeholder:"Bearer 令牌",tokenRequired:"访问令牌不能为空。"},ed={app:zl,nav:Hl,common:Ul,overview:Bl,sessions:Wl,chat:Vl,channels:ql,config:Kl,logs:Gl,hooks:Jl,mcp:Yl,skills:Xl,plugins:Ql,login:Zl},ln="prx-console-lang",Ra="en",yn={en:Dl,zh:ed};function Fn(e){return typeof e!="string"||e.trim().length===0?Ra:e.trim().toLowerCase().startsWith("zh")?"zh":"en"}function td(){var e;if(typeof window<"u"){const t=window.localStorage.getItem(ln);if(t)return Fn(t)}if(typeof navigator<"u"){const t=navigator.language||((e=navigator.languages)==null?void 0:e[0])||Ra;return Fn(t)}return Ra}function ps(e,t){return t.split(".").reduce((r,n)=>{if(!(!r||typeof r!="object"))return r[n]},e)}function ko(e){typeof document<"u"&&(document.documentElement.lang=e==="zh"?"zh-CN":"en")}function rd(e){typeof window<"u"&&window.localStorage.setItem(ln,e)}const Yr=kt({lang:td()});ko(Yr.lang);function wo(e){const t=Fn(e);Yr.lang!==t&&(Yr.lang=t,rd(t),ko(t))}function ta(){wo(Yr.lang==="en"?"zh":"en")}function ad(){if(typeof window>"u")return;const e=window.localStorage.getItem(ln);e&&wo(e)}function _(e,t={}){const r=yn[Yr.lang]??yn[Ra];let n=ps(r,e);if(typeof n!="string"&&(n=ps(yn[Ra],e)),typeof n!="string")return e;for(const[s,o]of Object.entries(t))n=n.replaceAll(`{${s}}`,String(o));return n}function So(){return typeof window>"u"?"/":window.location.pathname||"/"}function Sr(e,t=!1){if(typeof window>"u")return;e.startsWith("/")||(e=`/${e}`);const r=t?"replaceState":"pushState";window.location.pathname!==e&&(window.history[r]({},"",e),window.dispatchEvent(new PopStateEvent("popstate")))}function nd(e){if(typeof window>"u")return()=>{};const t=()=>{e(So())};return window.addEventListener("popstate",t),t(),()=>{window.removeEventListener("popstate",t)}}/**
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
 */const sd={xmlns:"http://www.w3.org/2000/svg",width:24,height:24,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor","stroke-width":2,"stroke-linecap":"round","stroke-linejoin":"round"};/**
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
 */const od=e=>{for(const t in e)if(t.startsWith("aria-")||t==="role"||t==="title")return!0;return!1};var id=Xi("<svg><!><!></svg>");function pt(e,t){$e(t,!0);const r=ea(t,"color",3,"currentColor"),n=ea(t,"size",3,24),s=ea(t,"strokeWidth",3,2),o=ea(t,"absoluteStrokeWidth",3,!1),l=ea(t,"iconNode",19,()=>[]),d=vt(t,["$$slots","$$events","$$legacy","name","color","size","strokeWidth","absoluteStrokeWidth","iconNode","children"]);var c=id();fs(c,(x,w)=>({...sd,...x,...d,width:n(),height:n(),stroke:r(),"stroke-width":w,class:["lucide-icon lucide",t.name&&`lucide-${t.name}`,t.class]}),[()=>!t.children&&!od(d)&&{"aria-hidden":"true"},()=>o()?Number(s())*24/Number(n()):s()]);var u=i(c);at(u,17,l,lt,(x,w)=>{var I=re(()=>Ma(a(w),2));let T=()=>a(I)[0],R=()=>a(I)[1];var N=Re(),O=xe(N);ol(O,T,!0,(Z,q)=>{fs(Z,()=>({...R()}))}),v(x,N)});var m=g(u);ft(m,()=>t.children??me),v(e,c),Ce()}function ld(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3.85 8.62a4 4 0 0 1 4.78-4.77 4 4 0 0 1 6.74 0 4 4 0 0 1 4.78 4.78 4 4 0 0 1 0 6.74 4 4 0 0 1-4.77 4.78 4 4 0 0 1-6.75 0 4 4 0 0 1-4.78-4.77 4 4 0 0 1 0-6.76Z"}],["path",{d:"m9 12 2 2 4-4"}]];pt(e,gt({name:"badge-check"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function bs(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M10 22V7a1 1 0 0 0-1-1H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-5a1 1 0 0 0-1-1H2"}],["rect",{x:"14",y:"2",width:"8",height:"8",rx:"1"}]];pt(e,gt({name:"blocks"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function dd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 8V4H8"}],["rect",{width:"16",height:"12",x:"4",y:"8",rx:"2"}],["path",{d:"M2 14h2"}],["path",{d:"M20 14h2"}],["path",{d:"M15 13v2"}],["path",{d:"M9 13v2"}]];pt(e,gt({name:"bot"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function cd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 18V5"}],["path",{d:"M15 13a4.17 4.17 0 0 1-3-4 4.17 4.17 0 0 1-3 4"}],["path",{d:"M17.598 6.5A3 3 0 1 0 12 5a3 3 0 1 0-5.598 1.5"}],["path",{d:"M17.997 5.125a4 4 0 0 1 2.526 5.77"}],["path",{d:"M18 18a4 4 0 0 0 2-7.464"}],["path",{d:"M19.967 17.483A4 4 0 1 1 12 18a4 4 0 1 1-7.967-.517"}],["path",{d:"M6 18a4 4 0 0 1-2-7.464"}],["path",{d:"M6.003 5.125a4 4 0 0 0-2.526 5.77"}]];pt(e,gt({name:"brain"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function ud(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M17 19a1 1 0 0 1-1-1v-2a2 2 0 0 1 2-2h2a2 2 0 0 1 2 2v2a1 1 0 0 1-1 1z"}],["path",{d:"M17 21v-2"}],["path",{d:"M19 14V6.5a1 1 0 0 0-7 0v11a1 1 0 0 1-7 0V10"}],["path",{d:"M21 21v-2"}],["path",{d:"M3 5V3"}],["path",{d:"M4 10a2 2 0 0 1-2-2V6a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2a2 2 0 0 1-2 2z"}],["path",{d:"M7 5V3"}]];pt(e,gt({name:"cable"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function fd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3 3v16a2 2 0 0 0 2 2h16"}],["path",{d:"M18 17V9"}],["path",{d:"M13 17V5"}],["path",{d:"M8 17v-3"}]];pt(e,gt({name:"chart-column"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function vd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["line",{x1:"12",x2:"12",y1:"8",y2:"12"}],["line",{x1:"12",x2:"12.01",y1:"16",y2:"16"}]];pt(e,gt({name:"circle-alert"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function gd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M21.801 10A10 10 0 1 1 17 3.335"}],["path",{d:"m9 11 3 3L22 4"}]];pt(e,gt({name:"circle-check-big"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function pd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["path",{d:"M12 6v6l4 2"}]];pt(e,gt({name:"clock"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function bd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m18 16 4-4-4-4"}],["path",{d:"m6 8-4 4 4 4"}],["path",{d:"m14.5 4-5 16"}]];pt(e,gt({name:"code-xml"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function yd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["ellipse",{cx:"12",cy:"5",rx:"9",ry:"3"}],["path",{d:"M3 5V19A9 3 0 0 0 21 19V5"}],["path",{d:"M3 12A9 3 0 0 0 21 12"}]];pt(e,gt({name:"database"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function hd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["line",{x1:"12",x2:"12",y1:"2",y2:"22"}],["path",{d:"M17 5H9.5a3.5 3.5 0 0 0 0 7h5a3.5 3.5 0 0 1 0 7H6"}]];pt(e,gt({name:"dollar-sign"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function md(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M15 6a9 9 0 0 0-9 9V3"}],["circle",{cx:"18",cy:"6",r:"3"}],["circle",{cx:"6",cy:"18",r:"3"}]];pt(e,gt({name:"git-branch"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function _d(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["path",{d:"M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"}],["path",{d:"M2 12h20"}]];pt(e,gt({name:"globe"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function xd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M2 9.5a5.5 5.5 0 0 1 9.591-3.676.56.56 0 0 0 .818 0A5.49 5.49 0 0 1 22 9.5c0 2.29-1.5 4-3 5.5l-5.492 5.313a2 2 0 0 1-3 .019L5 15c-1.5-1.5-3-3.2-3-5.5"}],["path",{d:"M3.22 13H9.5l.5-1 2 4.5 2-7 1.5 3.5h5.27"}]];pt(e,gt({name:"heart-pulse"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function kd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 2v4"}],["path",{d:"m16.2 7.8 2.9-2.9"}],["path",{d:"M18 12h4"}],["path",{d:"m16.2 16.2 2.9 2.9"}],["path",{d:"M12 18v4"}],["path",{d:"m4.9 19.1 2.9-2.9"}],["path",{d:"M2 12h4"}],["path",{d:"m4.9 4.9 2.9 2.9"}]];pt(e,gt({name:"loader"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function wd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z"}]];pt(e,gt({name:"message-square"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function Sd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401"}]];pt(e,gt({name:"moon"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function Ad(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m16 6-8.414 8.586a2 2 0 0 0 2.829 2.829l8.414-8.586a4 4 0 1 0-5.657-5.657l-8.379 8.551a6 6 0 1 0 8.485 8.485l8.379-8.551"}]];pt(e,gt({name:"paperclip"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function Ao(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"}],["path",{d:"M21 3v5h-5"}],["path",{d:"M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"}],["path",{d:"M8 16H3v5"}]];pt(e,gt({name:"refresh-cw"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function Ed(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m21 21-4.34-4.34"}],["circle",{cx:"11",cy:"11",r:"8"}]];pt(e,gt({name:"search"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function $d(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915"}],["circle",{cx:"12",cy:"12",r:"3"}]];pt(e,gt({name:"settings"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function Cd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"}]];pt(e,gt({name:"shield"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function Md(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"4"}],["path",{d:"M12 2v2"}],["path",{d:"M12 20v2"}],["path",{d:"m4.93 4.93 1.41 1.41"}],["path",{d:"m17.66 17.66 1.41 1.41"}],["path",{d:"M2 12h2"}],["path",{d:"M20 12h2"}],["path",{d:"m6.34 17.66-1.41 1.41"}],["path",{d:"m19.07 4.93-1.41 1.41"}]];pt(e,gt({name:"sun"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}function Nd(e,t){$e(t,!0);/**
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
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M4 14a1 1 0 0 1-.78-1.63l9.9-10.2a.5.5 0 0 1 .86.46l-1.92 6.02A1 1 0 0 0 13 10h7a1 1 0 0 1 .78 1.63l-9.9 10.2a.5.5 0 0 1-.86-.46l1.92-6.02A1 1 0 0 0 11 14z"}]];pt(e,gt({name:"zap"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Re(),d=xe(l);ft(d,()=>t.children??me),v(s,l)},$$slots:{default:!0}})),Ce()}var Pd=k('<p class="text-sm text-red-500 dark:text-red-400"> </p>'),Td=k('<div class="flex min-h-screen items-center justify-center bg-gray-50 px-4 py-8 text-gray-900 dark:bg-gray-900 dark:text-gray-100"><div class="w-full max-w-md rounded-xl border border-gray-200 bg-white p-6 shadow-xl shadow-black/10 dark:border-gray-700 dark:bg-gray-800 dark:shadow-black/30"><div class="flex items-center justify-between gap-3"><h1 class="text-2xl font-semibold tracking-tight"> </h1> <button type="button" class="rounded-lg border border-gray-300 bg-gray-50 px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <p class="mt-2 text-sm text-gray-500 dark:text-gray-400"> </p> <form class="mt-6 space-y-4"><label class="block text-sm font-medium text-gray-600 dark:text-gray-300" for="token"> </label> <input id="token" type="password" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-gray-900 outline-none ring-sky-500 transition focus:ring-2 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100" autocomplete="off"/> <!> <button type="submit" class="w-full rounded-lg bg-sky-600 px-4 py-2 font-medium text-white transition hover:bg-sky-500"> </button></form></div></div>');function Od(e,t){$e(t,!0);let r=F(""),n=F("");function s(M){var U;M.preventDefault();const C=a(r).trim();if(!C){y(n,_("login.tokenRequired"),!0);return}Sl(C),y(n,""),(U=t.onLogin)==null||U.call(t,C)}var o=Td(),l=i(o),d=i(l),c=i(d),u=i(c),m=g(c,2),x=i(m),w=g(d,2),I=i(w),T=g(w,2),R=i(T),N=i(R),O=g(R,2),Z=g(O,2);{var q=M=>{var C=Pd(),U=i(C);P(()=>p(U,a(n))),v(M,C)};K(Z,M=>{a(n)&&M(q)})}var E=g(Z,2),h=i(E);P((M,C,U,ie,fe,We)=>{p(u,M),ht(m,"aria-label",C),p(x,Yr.lang==="zh"?"中文 / EN":"EN / 中文"),p(I,U),p(N,ie),ht(O,"placeholder",fe),p(h,We)},[()=>_("login.title"),()=>_("app.language"),()=>_("login.hint"),()=>_("login.accessToken"),()=>_("login.placeholder"),()=>_("login.login")]),J("click",m,function(...M){ta==null||ta.apply(this,M)}),Fr("submit",T,s),Hr(O,()=>a(r),M=>y(r,M)),v(e,o),Ce()}or(["click"]);const hn="".trim(),Ya=hn.endsWith("/")?hn.slice(0,-1):hn;class ys extends Error{constructor(t,r){super(r),this.name="ApiError",this.status=t}}async function Id(e){return(e.headers.get("content-type")||"").includes("application/json")?e.json().catch(()=>null):e.text().catch(()=>null)}function Ld(e,t){return e&&typeof e=="object"&&typeof e.error=="string"?e.error:`Request failed (${t})`}async function Ct(e,t={}){const r=Fa(),n={Accept:"application/json",...t.headers};r&&(n.Authorization=`Bearer ${r}`),t.body&&!(t.body instanceof FormData)&&!n["Content-Type"]&&(n["Content-Type"]="application/json");const s=await fetch(`${Ya}${e}`,{...t,headers:n}),o=await Id(s);if(s.status===401)throw xo(),Sr("/",!0),new ys(401,"Unauthorized");if(!s.ok)throw new ys(s.status,Ld(o,s.status));return o}const wt={getStatus:()=>Ct("/api/status"),getSessions:()=>Ct("/api/sessions"),getSessionMessages:e=>Ct(`/api/sessions/${encodeURIComponent(e)}/messages`),sendMessage:(e,t)=>Ct(`/api/sessions/${encodeURIComponent(e)}/message`,{method:"POST",body:JSON.stringify({message:t})}),sendMessageWithMedia:(e,t,r=[])=>{if(!Array.isArray(r)||r.length===0)return wt.sendMessage(e,t);const n=new FormData;n.append("message",t);for(const s of r)n.append("files",s);return Ct(`/api/sessions/${encodeURIComponent(e)}/message`,{method:"POST",body:n})},getSessionMediaUrl:e=>{const t=new URLSearchParams({path:e}),r=Fa();return r&&t.set("token",r),`${Ya}/api/sessions/media?${t.toString()}`},getChannelsStatus:()=>Ct("/api/channels/status"),getConfig:()=>Ct("/api/config"),saveConfig:e=>Ct("/api/config",{method:"POST",body:JSON.stringify(e)}),getHooks:()=>Ct("/api/hooks"),getMcpServers:()=>Ct("/api/mcp/servers"),getSkills:()=>Ct("/api/skills"),discoverSkills:(e="github",t="")=>{const r=new URLSearchParams;return e&&r.set("source",e),t&&r.set("query",t),Ct(`/api/skills/discover?${r.toString()}`)},installSkill:(e,t)=>Ct("/api/skills/install",{method:"POST",body:JSON.stringify({url:e,name:t})}),uninstallSkill:e=>Ct(`/api/skills/${encodeURIComponent(e)}`,{method:"DELETE"}),toggleSkill:e=>Ct(`/api/skills/${encodeURIComponent(e)}/toggle`,{method:"PATCH"}),getPlugins:()=>Ct("/api/plugins"),reloadPlugin:e=>Ct(`/api/plugins/${encodeURIComponent(e)}/reload`,{method:"POST"})};function Fd(e){if(!Number.isFinite(e)||e<0)return"0s";const t=Math.floor(e/86400),r=Math.floor(e%86400/3600),n=Math.floor(e%3600/60),s=Math.floor(e%60),o=[];return t>0&&o.push(`${t}d`),(r>0||o.length>0)&&o.push(`${r}h`),(n>0||o.length>0)&&o.push(`${n}m`),o.push(`${s}s`),o.join(" ")}var Rd=k('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),jd=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Dd=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),zd=k('<div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><p class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400"> </p> <p class="mt-2 text-lg font-semibold text-gray-900 dark:text-gray-100"> </p></div>'),Hd=k('<p class="mt-3 text-sm text-gray-500 dark:text-gray-400"> </p>'),Ud=k('<li class="rounded-full border border-gray-300 bg-gray-50 px-3 py-1 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"> </li>'),Bd=k('<ul class="mt-3 flex flex-wrap gap-2"></ul>'),Wd=k('<div class="grid gap-4 sm:grid-cols-2 xl:grid-cols-5"></div> <div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><h3 class="text-sm font-semibold uppercase tracking-wide text-gray-600 dark:text-gray-300"> </h3> <!></div>',1),Vd=k('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function qd(e,t){$e(t,!0);let r=F(null),n=F(!0),s=F(""),o=F("");function l(h){return typeof h!="string"||h.length===0?_("common.unknown"):h.replaceAll("_"," ").split(" ").map(M=>M.charAt(0).toUpperCase()+M.slice(1)).join(" ")}function d(h){const M=`channels.names.${h}`,C=_(M);return C===M?l(h):C}const c=re(()=>{var h,M,C,U,ie;return[{label:_("overview.version"),value:((h=a(r))==null?void 0:h.version)??_("common.na")},{label:_("overview.uptime"),value:typeof((M=a(r))==null?void 0:M.uptime_seconds)=="number"?Fd(a(r).uptime_seconds):_("common.na")},{label:_("overview.model"),value:((C=a(r))==null?void 0:C.model)??_("common.na")},{label:_("overview.memoryBackend"),value:((U=a(r))==null?void 0:U.memory_backend)??_("common.na")},{label:_("overview.gatewayPort"),value:(ie=a(r))!=null&&ie.gateway_port?String(a(r).gateway_port):_("common.na")}]}),u=re(()=>{var h;return Array.isArray((h=a(r))==null?void 0:h.channels)?a(r).channels:[]});async function m(){try{const h=await wt.getStatus();y(r,h,!0),y(s,""),y(o,new Date().toLocaleTimeString(),!0)}catch(h){y(s,h instanceof Error?h.message:_("overview.loadFailed"),!0)}finally{y(n,!1)}}Ut(()=>{let h=!1;const M=async()=>{h||await m()};M();const C=setInterval(M,3e4);return()=>{h=!0,clearInterval(C)}});var x=Vd(),w=i(x),I=i(w),T=i(I),R=g(I,2);{var N=h=>{var M=Rd(),C=i(M);P(U=>p(C,U),[()=>_("common.updatedAt",{time:a(o)})]),v(h,M)};K(R,h=>{a(o)&&h(N)})}var O=g(w,2);{var Z=h=>{var M=jd(),C=i(M);P(U=>p(C,U),[()=>_("overview.loading")]),v(h,M)},q=h=>{var M=Dd(),C=i(M);P(()=>p(C,a(s))),v(h,M)},E=h=>{var M=Wd(),C=xe(M);at(C,21,()=>a(c),lt,(Y,be)=>{var ae=zd(),ze=i(ae),B=i(ze),V=g(ze,2),ve=i(V);P(()=>{p(B,a(be).label),p(ve,a(be).value)}),v(Y,ae)});var U=g(C,2),ie=i(U),fe=i(ie),We=g(ie,2);{var Me=Y=>{var be=Hd(),ae=i(be);P(ze=>p(ae,ze),[()=>_("overview.noChannelsConfigured")]),v(Y,be)},W=Y=>{var be=Bd();at(be,21,()=>a(u),lt,(ae,ze)=>{var B=Ud(),V=i(B);P(ve=>p(V,ve),[()=>d(a(ze))]),v(ae,B)}),v(Y,be)};K(We,Y=>{a(u).length===0?Y(Me):Y(W,-1)})}P(Y=>p(fe,Y),[()=>_("overview.configuredChannels")]),v(h,M)};K(O,h=>{a(n)?h(Z):a(s)?h(q,1):h(E,-1)})}P(h=>p(T,h),[()=>_("overview.title")]),v(e,x),Ce()}var Kd=k('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),Gd=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Jd=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Yd=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Xd=k('<tr class="cursor-pointer transition hover:bg-gray-50 dark:hover:bg-gray-700/40"><td class="px-4 py-3 font-mono text-xs"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td></tr>'),Qd=k('<div class="overflow-x-auto rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><table class="min-w-full divide-y divide-gray-200 text-sm dark:divide-gray-700"><thead class="bg-gray-50 text-left text-gray-600 dark:bg-gray-900/50 dark:text-gray-300"><tr><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th></tr></thead><tbody class="divide-y divide-gray-200 text-gray-700 dark:divide-gray-700 dark:text-gray-200"></tbody></table></div>'),Zd=k('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function ec(e,t){$e(t,!0);let r=F(kt([])),n=F(!0),s=F(""),o=F("");function l(h){return typeof h!="string"||h.length===0?_("common.unknown"):h.replaceAll("_"," ").split(" ").map(M=>M.charAt(0).toUpperCase()+M.slice(1)).join(" ")}function d(h){const M=`channels.names.${h}`,C=_(M);return C===M?l(h):C}async function c(){try{const h=await wt.getSessions();y(r,Array.isArray(h)?h:[],!0),y(s,""),y(o,new Date().toLocaleTimeString(),!0)}catch(h){y(s,h instanceof Error?h.message:_("sessions.loadFailed"),!0)}finally{y(n,!1)}}function u(h){Sr(`/chat/${encodeURIComponent(h)}`)}Ut(()=>{let h=!1;const M=async()=>{h||await c()};M();const C=setInterval(M,15e3);return()=>{h=!0,clearInterval(C)}});var m=Zd(),x=i(m),w=i(x),I=i(w),T=g(w,2);{var R=h=>{var M=Kd(),C=i(M);P(U=>p(C,U),[()=>_("common.updatedAt",{time:a(o)})]),v(h,M)};K(T,h=>{a(o)&&h(R)})}var N=g(x,2);{var O=h=>{var M=Gd(),C=i(M);P(U=>p(C,U),[()=>_("sessions.loading")]),v(h,M)},Z=h=>{var M=Jd(),C=i(M);P(()=>p(C,a(s))),v(h,M)},q=h=>{var M=Yd(),C=i(M);P(U=>p(C,U),[()=>_("sessions.none")]),v(h,M)},E=h=>{var M=Qd(),C=i(M),U=i(C),ie=i(U),fe=i(ie),We=i(fe),Me=g(fe),W=i(Me),Y=g(Me),be=i(Y),ae=g(Y),ze=i(ae),B=g(ae),V=i(B),ve=g(U);at(ve,21,()=>a(r),lt,(Se,ee)=>{var X=Xd(),ue=i(X),oe=i(ue),rt=g(ue),it=i(rt),Qe=g(rt),dt=i(Qe),j=g(Qe),Q=i(j),ye=g(j),st=i(ye);P((Ze,ot)=>{p(oe,a(ee).session_id),p(it,a(ee).sender),p(dt,Ze),p(Q,a(ee).message_count),p(st,ot)},[()=>d(a(ee).channel),()=>a(ee).last_message_preview||_("common.empty")]),J("click",X,()=>u(a(ee).session_id)),v(Se,X)}),P((Se,ee,X,ue,oe)=>{p(We,Se),p(W,ee),p(be,X),p(ze,ue),p(V,oe)},[()=>_("sessions.sessionId"),()=>_("sessions.sender"),()=>_("sessions.channel"),()=>_("sessions.messages"),()=>_("sessions.lastMessage")]),v(h,M)};K(N,h=>{a(n)?h(O):a(s)?h(Z,1):a(r).length===0?h(q,2):h(E,-1)})}P(h=>p(I,h),[()=>_("sessions.title")]),v(e,m),Ce()}or(["click"]);var tc=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),rc=k('<p class="mb-3 rounded-lg border border-blue-500/40 bg-blue-500/15 px-3 py-2 text-sm text-blue-700 dark:text-blue-200"> </p>'),ac=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),nc=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),sc=k('<p class="whitespace-pre-wrap break-words text-sm"> </p>'),oc=k('<img alt="Attachment" class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 object-contain dark:border-gray-600/40" loading="lazy"/>'),ic=k('<video controls="" class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 dark:border-gray-600/40"></video>',2),lc=k("<div></div>"),dc=k('<div class="space-y-3"></div>'),cc=k('<img class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"/>'),uc=k('<video class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"></video>',2),fc=k('<div class="flex h-12 w-12 items-center justify-center rounded border border-gray-300 bg-gray-100 text-lg text-gray-700 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200">DOC</div>'),vc=k('<div class="flex items-center gap-2 rounded-md border border-gray-200 bg-white/90 p-2 dark:border-gray-700 dark:bg-gray-800/90"><!> <div class="min-w-0 flex-1"><p class="truncate text-sm text-gray-900 dark:text-gray-100"> </p> <p class="truncate text-xs text-gray-500 dark:text-gray-400"> </p></div> <button type="button" class="rounded px-2 py-1 text-xs text-gray-600 hover:bg-gray-200 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-white">Remove</button></div>'),gc=k('<div class="mb-3 space-y-2 rounded-lg border border-gray-200 bg-gray-50/70 p-2.5 dark:border-gray-700 dark:bg-gray-900/70"><p class="text-xs text-gray-600 dark:text-gray-300"> </p> <div class="max-h-44 space-y-2 overflow-y-auto pr-1"></div></div>'),pc=k('<section class="flex h-[calc(100vh-10rem)] flex-col gap-4"><div class="flex items-center justify-between"><div class="min-w-0"><h2 class="text-2xl font-semibold"> </h2> <p class="truncate font-mono text-xs text-gray-500 dark:text-gray-400"> </p></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!> <div class="flex min-h-0 flex-1 flex-col rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800" role="region" aria-label="Chat messages"><div><!> <!></div> <form class="border-t border-gray-200 p-3 dark:border-gray-700"><input type="file" class="hidden" multiple="" accept="image/*,video/*,.pdf,.doc,.docx,.txt,.md,.csv,.json,.zip,.tar,.gz,.rar,.ppt,.pptx,.xls,.xlsx"/> <!> <div class="flex items-end gap-2"><textarea rows="2" class="min-h-[2.75rem] flex-1 resize-y rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-900 outline-none focus:border-blue-500 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100"></textarea> <button type="button" title="Attach files" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-600 hover:border-gray-400 hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:border-gray-500 dark:hover:bg-gray-700"><!></button> <button type="submit" class="rounded-lg bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"> </button></div></form></div></section>');function bc(e,t){$e(t,!0);const r=10,n=/\[(IMAGE|VIDEO):([^\]]+)\]|(data:(?:image|video)\/[a-zA-Z0-9.+-]+;base64,[a-zA-Z0-9+/=]+)/gi;let s=ea(t,"sessionId",3,""),o=F(kt([])),l=F(""),d=F(!0),c=F(!1),u=F(""),m=F(null),x=F(null),w=F(kt([])),I=F(!1),T=0;function R(){Sr("/sessions")}function N(S){return S==="user"?"ml-auto max-w-[85%] rounded-2xl rounded-br-md bg-blue-600 px-4 py-2 text-white":S==="assistant"?"mr-auto max-w-[85%] rounded-2xl rounded-bl-md bg-gray-200 px-4 py-2 text-gray-900 dark:bg-gray-700 dark:text-gray-100":"mx-auto max-w-[90%] rounded-lg bg-gray-100/60 px-3 py-1.5 text-center text-xs text-gray-500 dark:bg-gray-800/60 dark:text-gray-400"}function O(S){return((S==null?void 0:S.type)||"").startsWith("image/")}function Z(S){return((S==null?void 0:S.type)||"").startsWith("video/")}function q(S){if(!Number.isFinite(S)||S<=0)return"0 B";const f=["B","KB","MB","GB"];let b=S,A=0;for(;b>=1024&&A<f.length-1;)b/=1024,A+=1;return`${b.toFixed(A===0?0:1)} ${f[A]}`}function E(S){return typeof S=="string"&&S.trim().length>0?S:"unknown"}function h(S){const f=O(S),b=Z(S);return{id:`${S.name}-${S.lastModified}-${Math.random().toString(36).slice(2)}`,file:S,name:S.name,size:S.size,type:E(S.type),isImage:f,isVideo:b,previewUrl:f||b?URL.createObjectURL(S):""}}function M(S){S&&typeof S.previewUrl=="string"&&S.previewUrl.startsWith("blob:")&&URL.revokeObjectURL(S.previewUrl)}function C(){for(const S of a(w))M(S);y(w,[],!0),a(x)&&(a(x).value="")}function U(S){if(!S||S.length===0||a(c))return;const f=Array.from(S),b=[],A=Math.max(0,r-a(w).length);for(const L of f.slice(0,A))b.push(h(L));y(w,[...a(w),...b],!0)}function ie(S){const f=a(w).find(b=>b.id===S);f&&M(f),y(w,a(w).filter(b=>b.id!==S),!0)}function fe(){var S;a(c)||(S=a(x))==null||S.click()}function We(S){var f;U((f=S.currentTarget)==null?void 0:f.files),a(x)&&(a(x).value="")}function Me(S){S.preventDefault(),!a(c)&&(T+=1,y(I,!0))}function W(S){S.preventDefault(),!a(c)&&S.dataTransfer&&(S.dataTransfer.dropEffect="copy")}function Y(S){S.preventDefault(),T=Math.max(0,T-1),T===0&&y(I,!1)}function be(S){var f;S.preventDefault(),T=0,y(I,!1),U((f=S.dataTransfer)==null?void 0:f.files)}function ae(S){const f=(S||"").trim();if(!f)return"";const b=f.toLowerCase();return b.startsWith("data:image/")||b.startsWith("data:video/")||b.startsWith("http://")||b.startsWith("https://")?f:wt.getSessionMediaUrl(f)}function ze(S,f){const b=(f||"").trim().toLowerCase();return S==="VIDEO"||b.startsWith("data:video/")?"video":b.startsWith("data:image/")?"image":[".mp4",".webm",".mov",".m4v",".ogg"].some(L=>b.endsWith(L))?"video":"image"}function B(S){if(typeof S!="string"||S.length===0)return[];const f=[];n.lastIndex=0;let b=0,A;for(;(A=n.exec(S))!==null;){A.index>b&&f.push({id:`text-${b}`,kind:"text",value:S.slice(b,A.index)});const L=(A[1]||"").toUpperCase(),z=(A[2]||A[3]||"").trim();if(z){const H=ze(L,z);f.push({id:`${H}-${A.index}`,kind:H,value:z})}b=n.lastIndex}return b<S.length&&f.push({id:`text-tail-${b}`,kind:"text",value:S.slice(b)}),f}async function V(){await co(),a(m)&&(a(m).scrollTop=a(m).scrollHeight)}async function ve(){try{const S=await wt.getSessionMessages(s());y(o,Array.isArray(S)?S:[],!0),y(u,""),await V()}catch(S){y(u,S instanceof Error?S.message:_("chat.loadFailed"),!0)}finally{y(d,!1)}}async function Se(){const S=a(l).trim(),f=a(w).map(A=>A.file);if(S.length===0&&f.length===0||a(c))return;y(c,!0),y(l,""),y(u,"");const b=f.length>0;b||(y(o,[...a(o),{role:"user",content:S}],!0),await V());try{const A=b?await wt.sendMessageWithMedia(s(),S,f):await wt.sendMessage(s(),S);b?await ve():A&&typeof A.reply=="string"&&A.reply.length>0&&y(o,[...a(o),{role:"assistant",content:A.reply}],!0),C()}catch(A){y(u,A instanceof Error?A.message:_("chat.sendFailed"),!0),await ve()}finally{y(c,!1),await V()}}function ee(S){S.preventDefault(),Se()}Ut(()=>{let S=!1;return(async()=>{S||(y(d,!0),await ve())})(),()=>{S=!0}}),xl(()=>{for(const S of a(w))M(S)});var X=pc(),ue=i(X),oe=i(ue),rt=i(oe),it=i(rt),Qe=g(rt,2),dt=i(Qe),j=g(oe,2),Q=i(j),ye=g(ue,2);{var st=S=>{var f=tc(),b=i(f);P(()=>p(b,a(u))),v(S,f)};K(ye,S=>{a(u)&&S(st)})}var Ze=g(ye,2),ot=i(Ze),bt=i(ot);{var Ie=S=>{var f=rc(),b=i(f);P(()=>p(b,`Drop files to attach (${a(w).length??""}/10 selected)`)),v(S,f)};K(bt,S=>{a(I)&&S(Ie)})}var Ve=g(bt,2);{var ct=S=>{var f=ac(),b=i(f);P(A=>p(b,A),[()=>_("chat.loading")]),v(S,f)},He=S=>{var f=nc(),b=i(f);P(A=>p(b,A),[()=>_("chat.empty")]),v(S,f)},yt=S=>{var f=dc();at(f,21,()=>a(o),lt,(b,A)=>{var L=lc();at(L,21,()=>B(a(A).content),z=>z.id,(z,H)=>{var de=Re(),Pe=xe(de);{var Fe=ke=>{var je=Re(),ce=xe(je);{var ne=Ue=>{var Ee=sc(),te=i(Ee);P(()=>p(te,a(H).value)),v(Ue,Ee)},ge=re(()=>a(H).value.trim().length>0);K(ce,Ue=>{a(ge)&&Ue(ne)})}v(ke,je)},Ye=ke=>{var je=oc();P(ce=>ht(je,"src",ce),[()=>ae(a(H).value)]),v(ke,je)},Ae=ke=>{var je=ic();P(ce=>ht(je,"src",ce),[()=>ae(a(H).value)]),v(ke,je)};K(Pe,ke=>{a(H).kind==="text"?ke(Fe):a(H).kind==="image"?ke(Ye,1):a(H).kind==="video"&&ke(Ae,2)})}v(z,de)}),P(z=>tt(L,1,z),[()=>yo(N(a(A).role))]),v(b,L)}),v(S,f)};K(Ve,S=>{a(d)?S(ct):a(o).length===0?S(He,1):S(yt,-1)})}Ln(ot,S=>y(m,S),()=>a(m));var le=g(ot,2),Le=i(le);Ln(Le,S=>y(x,S),()=>a(x));var he=g(Le,2);{var Ne=S=>{var f=gc(),b=i(f),A=i(b),L=g(b,2);at(L,21,()=>a(w),z=>z.id,(z,H)=>{var de=vc(),Pe=i(de);{var Fe=Ee=>{var te=cc();P(()=>{ht(te,"src",a(H).previewUrl),ht(te,"alt",a(H).name)}),v(Ee,te)},Ye=Ee=>{var te=uc();te.muted=!0,P(()=>ht(te,"src",a(H).previewUrl)),v(Ee,te)},Ae=Ee=>{var te=fc();v(Ee,te)};K(Pe,Ee=>{a(H).isImage?Ee(Fe):a(H).isVideo?Ee(Ye,1):Ee(Ae,-1)})}var ke=g(Pe,2),je=i(ke),ce=i(je),ne=g(je,2),ge=i(ne),Ue=g(ke,2);P(Ee=>{p(ce,a(H).name),p(ge,`${a(H).type??""} · ${Ee??""}`)},[()=>q(a(H).size)]),J("click",Ue,()=>ie(a(H).id)),v(z,de)}),P(()=>p(A,`Attachments (${a(w).length??""}/10)`)),v(S,f)};K(he,S=>{a(w).length>0&&S(Ne)})}var D=g(he,2),G=i(D),qe=g(G,2),Je=i(qe);Ad(Je,{size:16});var ut=g(qe,2),mt=i(ut);P((S,f,b,A,L,z)=>{p(it,S),p(dt,`${f??""}: ${s()??""}`),p(Q,b),tt(ot,1,`flex-1 overflow-y-auto p-4 ${a(I)?"bg-blue-500/10 ring-1 ring-inset ring-blue-500/40":""}`),ht(G,"placeholder",A),qe.disabled=a(c)||a(w).length>=r,ut.disabled=L,p(mt,z)},[()=>_("chat.title"),()=>_("chat.session"),()=>_("chat.back"),()=>_("chat.inputPlaceholder"),()=>a(c)||!a(l).trim()&&a(w).length===0,()=>a(c)?_("chat.sending"):_("chat.send")]),J("click",j,R),Fr("dragenter",Ze,Me),Fr("dragover",Ze,W),Fr("dragleave",Ze,Y),Fr("drop",Ze,be),Fr("submit",le,ee),J("change",Le,We),Hr(G,()=>a(l),S=>y(l,S)),J("click",qe,fe),v(e,X),Ce()}or(["click","change"]);var yc=k('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),hc=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),mc=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),_c=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),xc=k('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-3 text-sm text-gray-500 dark:text-gray-400"> </p></article>'),kc=k('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),wc=k('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function Sc(e,t){$e(t,!0);let r=F(kt([])),n=F(!0),s=F(""),o=F("");function l(E){return typeof E!="string"||E.length===0?_("common.unknown"):E.replaceAll("_"," ").split(" ").map(h=>h.charAt(0).toUpperCase()+h.slice(1)).join(" ")}function d(E){const h=`channels.names.${E}`,M=_(h);return M===h?l(E):M}async function c(){try{const E=await wt.getChannelsStatus();y(r,Array.isArray(E==null?void 0:E.channels)?E.channels:[],!0),y(s,""),y(o,new Date().toLocaleTimeString(),!0)}catch(E){y(s,E instanceof Error?E.message:_("channels.loadFailed"),!0)}finally{y(n,!1)}}Ut(()=>{let E=!1;const h=async()=>{E||await c()};h();const M=setInterval(h,3e4);return()=>{E=!0,clearInterval(M)}});var u=wc(),m=i(u),x=i(m),w=i(x),I=g(x,2);{var T=E=>{var h=yc(),M=i(h);P(C=>p(M,C),[()=>_("common.updatedAt",{time:a(o)})]),v(E,h)};K(I,E=>{a(o)&&E(T)})}var R=g(m,2);{var N=E=>{var h=hc(),M=i(h);P(C=>p(M,C),[()=>_("channels.loading")]),v(E,h)},O=E=>{var h=mc(),M=i(h);P(()=>p(M,a(s))),v(E,h)},Z=E=>{var h=_c(),M=i(h);P(C=>p(M,C),[()=>_("channels.noChannels")]),v(E,h)},q=E=>{var h=kc();at(h,21,()=>a(r),lt,(M,C)=>{var U=xc(),ie=i(U),fe=i(ie),We=i(fe),Me=g(fe,2),W=i(Me),Y=g(ie,2),be=i(Y);P((ae,ze,B,V)=>{p(We,ae),tt(Me,1,`rounded-full px-2 py-1 text-xs font-medium ${a(C).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),p(W,ze),p(be,`${B??""}: ${V??""}`)},[()=>d(a(C).name),()=>a(C).enabled?_("common.enabled"):_("common.disabled"),()=>_("channels.type"),()=>d(a(C).type)]),v(M,U)}),v(E,h)};K(R,E=>{a(n)?E(N):a(s)?E(O,1):a(r).length===0?E(Z,2):E(q,-1)})}P(E=>p(w,E),[()=>_("channels.title")]),v(e,u),Ce()}function mn(e){return e.replaceAll("&","&amp;").replaceAll("<","&lt;").replaceAll(">","&gt;").replaceAll('"',"&quot;")}const hs=/(\"(\\u[0-9a-fA-F]{4}|\\[^u]|[^\\\"])*\"(?:\s*:)?|\btrue\b|\bfalse\b|\bnull\b|-?\d+(?:\.\d+)?(?:[eE][+\-]?\d+)?)/g;function Ac(e){return e.startsWith('"')?e.endsWith(":")?"text-sky-300":"text-emerald-300":e==="true"||e==="false"?"text-amber-300":e==="null"?"text-fuchsia-300":"text-violet-300"}function Ec(e){if(!e)return"";let t="",r=0;hs.lastIndex=0;for(const n of e.matchAll(hs)){const s=n.index??0,o=n[0];t+=mn(e.slice(r,s)),t+=`<span class="${Ac(o)}">${mn(o)}</span>`,r=s+o.length}return t+=mn(e.slice(r)),t}var $c=k('<span class="ml-1.5 text-xs text-sky-500 dark:text-sky-400">已修改</span>'),Cc=k('<button type="button"><span></span></button>'),Mc=k("<option> </option>"),Nc=k('<select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select>'),Pc=k('<input type="number" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/>'),Tc=k('<div class="flex gap-1"><input type="text" class="flex-1 rounded border border-gray-300 bg-white px-2 py-1 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <button type="button" class="rounded border border-gray-300 bg-white px-2 py-1 text-xs text-red-500 hover:bg-red-500/10 dark:border-gray-600 dark:bg-gray-800 dark:text-red-400">×</button></div>'),Oc=k('<div class="space-y-1.5"><!> <button type="button" class="rounded border border-dashed border-gray-300 px-2 py-1 text-xs text-gray-500 hover:border-sky-500 hover:text-sky-500 dark:border-gray-600 dark:text-gray-400 dark:hover:border-sky-500 dark:hover:text-sky-400">+ 添加</button></div>'),Ic=k('<div class="flex gap-1"><input class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <button type="button" class="rounded-lg border border-gray-300 bg-white px-2 text-xs text-gray-500 hover:text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-400 dark:hover:text-gray-200"> </button></div>'),Lc=k('<input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/>'),Fc=k('<div><div class="flex items-start justify-between gap-3"><div class="flex-1 min-w-0"><label class="block text-sm font-medium text-gray-700 dark:text-gray-200"> <!></label> <p class="mt-0.5 text-xs text-gray-400 dark:text-gray-500"> </p></div> <div class="flex-shrink-0 w-64"><!></div></div></div>'),Rc=k('<button type="button"><span></span></button>'),jc=k('<input type="number" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/>'),Dc=k('<div class="flex gap-1"><input type="text" class="flex-1 rounded border border-gray-300 bg-white px-2 py-1 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <button type="button" class="rounded border border-gray-300 px-2 py-1 text-xs text-red-500 hover:bg-red-500/10 dark:border-gray-600 dark:bg-gray-800">×</button></div>'),zc=k('<div class="space-y-1.5"><!> <button type="button" class="rounded border border-dashed border-gray-300 px-2 py-1 text-xs text-gray-500 hover:border-sky-500 hover:text-sky-500 dark:border-gray-600">+ 添加</button></div>'),Hc=k('<div class="flex gap-1"><input class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200" placeholder="未设置"/> <button type="button" class="rounded-lg border border-gray-300 bg-white px-2 text-xs text-gray-500 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-400"> </button></div>'),Uc=k('<input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200" placeholder="未设置"/>'),Bc=k('<textarea class="w-full rounded-lg border border-gray-300 bg-white font-mono text-xs leading-relaxed p-2 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 resize-y"></textarea>'),Wc=k('<span class="text-xs text-sky-500">已修改</span>'),Vc=k('<div class="mb-2 flex items-center gap-2"><!> <span class="font-mono text-xs font-medium text-gray-600 dark:text-gray-300"> </span> <!></div> <!>',1),qc=k('<span class="ml-1.5 text-xs text-sky-500">已修改</span>'),Kc=k('<div class="flex items-center justify-between gap-3"><span class="min-w-0 flex-1 font-mono text-sm text-gray-700 dark:text-gray-200"> <!></span> <div class="w-56 flex-shrink-0"><!></div></div>'),Gc=k("<div><!></div>"),Jc=k('<span class="inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),Yc=k('<span class="ml-auto text-xs text-gray-400"> </span>'),Xc=k('<details class="rounded-lg border border-gray-200 dark:border-gray-700"><summary class="cursor-pointer select-none flex items-center gap-2 px-3 py-2 text-sm font-medium text-gray-700 dark:text-gray-200 hover:bg-gray-50 dark:hover:bg-gray-700/50 rounded-lg"><span class="font-mono"> </span> <!> <!></summary> <div class="border-t border-gray-200 px-3 py-2 space-y-2 dark:border-gray-700"><!></div></details>'),Qc=k('<div class="space-y-2"></div>'),Zc=k('<p class="text-sm text-gray-500 dark:text-gray-400">加载配置中...</p>'),eu=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),tu=k('<div class="overflow-x-auto rounded-xl border border-gray-200 bg-gray-50 p-4 dark:border-gray-700 dark:bg-gray-950"><pre class="text-sm leading-6 text-gray-700 dark:text-gray-200"><code><!></code></pre></div>'),ru=k('<span class="ml-2 inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),au=k('<div class="mt-2 border-t border-gray-100 pt-3 dark:border-gray-700/60"><p class="mb-2 text-xs font-medium uppercase tracking-wider text-gray-400 dark:text-gray-500">其他子配置</p> <div class="space-y-2"></div></div>'),nu=k('<details class="group rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><summary class="cursor-pointer select-none px-4 py-3 text-base font-semibold text-gray-900 flex items-center gap-2 dark:text-gray-100"><!> <span> </span> <!></summary> <div class="border-t border-gray-200 px-4 py-3 space-y-3 dark:border-gray-700"><!> <!></div></details>'),su=k('<span class="inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),ou=k('<details class="rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><summary class="cursor-pointer select-none px-4 py-3 flex items-center gap-2 dark:text-gray-100"><!> <span class="font-mono text-sm font-semibold text-gray-800 dark:text-gray-100"> </span> <!> <span class="ml-auto text-xs text-gray-400 dark:text-gray-500"> </span></summary> <div class="border-t border-gray-200 px-4 py-3 dark:border-gray-700"><!></div></details>'),iu=k('<div class="pt-1"><p class="mb-2 px-1 text-xs font-semibold uppercase tracking-wider text-gray-400 dark:text-gray-500">自动发现的配置项</p> <div class="space-y-3"></div></div>'),lu=k('<div class="space-y-3"><!> <!></div>'),du=k('<div class="flex items-start gap-2 text-xs flex-wrap"><span class="flex-shrink-0 text-gray-400 dark:text-gray-500"> </span> <span class="font-medium text-gray-600 dark:text-gray-300"> </span> <span class="text-red-500 line-through dark:text-red-400 break-all"> </span> <span class="text-gray-400 dark:text-gray-600">→</span> <span class="text-green-600 dark:text-green-400 break-all"> </span></div>'),cu=k('<div class="mx-auto mt-3 max-w-5xl rounded-lg border border-gray-200 bg-gray-50 p-3 dark:border-gray-700 dark:bg-gray-950"><p class="mb-2 text-xs font-medium text-gray-500 dark:text-gray-400">变更详情</p> <div class="space-y-1.5 max-h-48 overflow-y-auto"></div></div>'),uu=k('<div class="fixed bottom-0 left-0 right-0 z-50 border-t border-gray-200 bg-white/95 px-6 py-3 backdrop-blur-sm dark:border-gray-700 dark:bg-gray-900/95"><div class="mx-auto flex max-w-5xl items-center justify-between gap-4"><div class="flex items-center gap-3"><span class="text-sm text-sky-600 dark:text-sky-400"> </span> <button type="button" class="text-sm text-gray-500 underline hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"> </button></div> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-4 py-2 text-sm text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700">放弃修改</button> <button type="button" class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"> </button></div></div> <!></div>'),fu=k("<div> </div>"),vu=k('<section class="space-y-4 pb-24"><div class="flex items-center justify-between gap-4"><h2 class="text-2xl font-semibold"> </h2> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700">复制 JSON</button></div></div> <!> <!> <!></section>');function gu(e,t){$e(t,!0);const r=(f,b=me,A=me)=>{const L=re(()=>Y(b())),z=re(()=>a(Se).has(b())),H=re(()=>a(O).has(b()));var de=Fc(),Pe=i(de),Fe=i(Pe),Ye=i(Fe),Ae=i(Ye),ke=g(Ae);{var je=we=>{var se=$c();v(we,se)};K(ke,we=>{a(z)&&we(je)})}var ce=g(Ye,2),ne=i(ce),ge=g(Fe,2),Ue=i(ge);{var Ee=we=>{var se=Cc(),Xe=i(se);P(()=>{tt(se,1,`relative inline-flex h-6 w-11 items-center rounded-full transition-colors ${a(L)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),tt(Xe,1,`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${a(L)?"translate-x-6":"translate-x-1"}`)}),J("click",se,()=>oe(b(),!a(L))),v(we,se)},te=we=>{var se=Nc();at(se,21,()=>A().options,lt,(Ke,_t)=>{var St=Mc(),Lt=i(St),Ft={};P(()=>{p(Lt,a(_t)||"(默认)"),Ft!==(Ft=a(_t))&&(St.value=(St.__value=a(_t))??"")}),v(Ke,St)});var Xe;Jn(se),P(()=>{Xe!==(Xe=a(L)??A().default)&&(se.value=(se.__value=a(L)??A().default)??"",La(se,a(L)??A().default))}),J("change",se,Ke=>oe(b(),Ke.target.value)),v(we,se)},nt=we=>{var se=Pc();P(Xe=>{yr(se,a(L)??A().default),ht(se,"min",A().min),ht(se,"max",A().max),ht(se,"step",A().step??1),ht(se,"placeholder",Xe)},[()=>String(A().default)]),J("input",se,Xe=>{const Ke=A().step&&A().step<1?parseFloat(Xe.target.value):parseInt(Xe.target.value,10);isNaN(Ke)||oe(b(),Ke)}),v(we,se)},pe=we=>{var se=Oc(),Xe=i(se);{var Ke=Lt=>{var Ft=Re(),Qr=xe(Ft);at(Qr,17,()=>a(L),lt,(Qt,ka,wa)=>{var Ua=Tc(),Sa=i(Ua),dn=g(Sa,2);P(()=>yr(Sa,a(ka))),J("input",Sa,cn=>Qe(b(),wa,cn.target.value)),J("click",dn,()=>it(b(),wa)),v(Qt,Ua)}),v(Lt,Ft)},_t=re(()=>Array.isArray(a(L)));K(Xe,Lt=>{a(_t)&&Lt(Ke)})}var St=g(Xe,2);J("click",St,()=>rt(b())),v(we,se)},Te=we=>{var se=Ic(),Xe=i(se),Ke=g(Xe,2),_t=i(Ke);P(()=>{ht(Xe,"type",a(H)?"text":"password"),yr(Xe,a(L)??""),ht(Xe,"placeholder",A().default||"未设置"),p(_t,a(H)?"隐藏":"显示")}),J("input",Xe,St=>oe(b(),St.target.value)),J("click",Ke,()=>dt(b())),v(we,se)},et=we=>{var se=Lc();P(()=>{yr(se,a(L)??""),ht(se,"placeholder",A().default||"未设置")}),J("input",se,Xe=>oe(b(),Xe.target.value)),v(we,se)};K(Ue,we=>{A().type==="bool"?we(Ee):A().type==="enum"?we(te,1):A().type==="number"?we(nt,2):A().type==="array"?we(pe,3):A().sensitive?we(Te,4):we(et,-1)})}P(()=>{tt(de,1,`rounded-lg border p-3 transition-colors ${a(z)?"border-sky-500/50 bg-sky-500/5":"border-gray-200 bg-gray-50/40 dark:border-gray-700 dark:bg-gray-900/40"}`),p(Ae,`${A().label??""} `),p(ne,A().desc)}),v(f,de)},n=(f,b=me,A=me)=>{const L=re(()=>M(b().split(".").pop()??"")),z=re(()=>a(O).has(b()));var H=Re(),de=xe(H);{var Pe=ce=>{var ne=Rc(),ge=i(ne);P(()=>{tt(ne,1,`relative inline-flex h-6 w-11 items-center rounded-full transition-colors ${A()?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),tt(ge,1,`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${A()?"translate-x-6":"translate-x-1"}`)}),J("click",ne,()=>oe(b(),!A())),v(ce,ne)},Fe=ce=>{var ne=jc();P(()=>yr(ne,A())),J("input",ne,ge=>{const Ue=parseFloat(ge.target.value);isNaN(Ue)||oe(b(),Ue)}),v(ce,ne)},Ye=ce=>{var ne=zc(),ge=i(ne);at(ge,17,A,lt,(Ee,te,nt)=>{var pe=Dc(),Te=i(pe),et=g(Te,2);P(()=>yr(Te,a(te))),J("input",Te,we=>{const se=[...Me(a(c),b())||[]];se[nt]=we.target.value,oe(b(),se)}),J("click",et,()=>{const we=(Me(a(c),b())||[]).filter((se,Xe)=>Xe!==nt);oe(b(),we)}),v(Ee,pe)});var Ue=g(ge,2);J("click",Ue,()=>{const Ee=[...Me(a(c),b())||[],""];oe(b(),Ee)}),v(ce,ne)},Ae=re(()=>Array.isArray(A())),ke=ce=>{var ne=Hc(),ge=i(ne),Ue=g(ge,2),Ee=i(Ue);P(()=>{ht(ge,"type",a(z)?"text":"password"),yr(ge,A()??""),p(Ee,a(z)?"隐藏":"显示")}),J("input",ge,te=>oe(b(),te.target.value)),J("click",Ue,()=>dt(b())),v(ce,ne)},je=ce=>{var ne=Uc();P(()=>yr(ne,A()??"")),J("input",ne,ge=>oe(b(),ge.target.value)),v(ce,ne)};K(de,ce=>{typeof A()=="boolean"?ce(Pe):typeof A()=="number"?ce(Fe,1):a(Ae)?ce(Ye,2):a(L)?ce(ke,3):ce(je,-1)})}v(f,H)},s=(f,b=me,A=me)=>{const L=re(()=>JSON.stringify(A(),null,2)),z=re(()=>Math.min(15,(a(L).match(/\n/g)||[]).length+2));var H=Bc();P(()=>{yr(H,a(L)),ht(H,"rows",a(z))}),Fr("blur",H,de=>{try{const Pe=JSON.parse(de.target.value);oe(b(),Pe)}catch{de.target.value=JSON.stringify(Me(a(c),b())??A(),null,2)}}),v(f,H)},o=(f,b=me,A=me,L=me)=>{const z=re(()=>Me(a(c),b())??L()),H=re(()=>a(Se).has(b()));var de=Gc(),Pe=i(de);{var Fe=ke=>{var je=Vc(),ce=xe(je),ne=i(ce);bd(ne,{size:13,class:"flex-shrink-0 text-gray-400"});var ge=g(ne,2),Ue=i(ge),Ee=g(ge,2);{var te=pe=>{var Te=Wc();v(pe,Te)};K(Ee,pe=>{a(H)&&pe(te)})}var nt=g(ce,2);s(nt,b,()=>a(z)),P(()=>p(Ue,A())),v(ke,je)},Ye=re(()=>Q(a(z))),Ae=ke=>{var je=Kc(),ce=i(je),ne=i(ce),ge=g(ne);{var Ue=nt=>{var pe=qc();v(nt,pe)};K(ge,nt=>{a(H)&&nt(Ue)})}var Ee=g(ce,2),te=i(Ee);n(te,b,()=>a(z)),P(()=>p(ne,`${A()??""} `)),v(ke,je)};K(Pe,ke=>{a(Ye)?ke(Fe):ke(Ae,-1)})}P(()=>tt(de,1,`rounded-lg border p-3 transition-colors ${a(H)?"border-sky-500/50 bg-sky-500/5":"border-gray-200 bg-gray-50/40 dark:border-gray-700 dark:bg-gray-900/40"}`)),v(f,de)},l=(f,b=me,A=me,L=me)=>{const z=re(()=>E(L())),H=re(()=>ee(b()));var de=Xc(),Pe=i(de),Fe=i(Pe),Ye=i(Fe),Ae=g(Fe,2);{var ke=te=>{var nt=Jc();v(te,nt)};K(Ae,te=>{a(H)&&te(ke)})}var je=g(Ae,2);{var ce=te=>{var nt=Yc(),pe=i(nt);P(Te=>p(pe,Te),[()=>h(L())]),v(te,nt)};K(je,te=>{a(z)||te(ce)})}var ne=g(Pe,2),ge=i(ne);{var Ue=te=>{var nt=Re(),pe=xe(nt);at(pe,17,()=>Object.entries(L()),lt,(Te,et)=>{var we=re(()=>Ma(a(et),2));let se=()=>a(we)[0],Xe=()=>a(we)[1];const Ke=re(()=>`${b()}.${se()}`);var _t=Re(),St=xe(_t);{var Lt=Qt=>{o(Qt,()=>a(Ke),se,Xe)},Ft=re(()=>E(Xe())),Qr=Qt=>{o(Qt,()=>a(Ke),se,Xe)};K(St,Qt=>{a(Ft)?Qt(Lt):Qt(Qr,-1)})}v(Te,_t)}),v(te,nt)},Ee=te=>{o(te,b,A,L)};K(ge,te=>{a(z)?te(Ue):te(Ee,-1)})}P(()=>p(Ye,A())),v(f,de)},d=(f,b=me,A=me)=>{var L=Re(),z=xe(L);{var H=Ae=>{var ke=Qc();at(ke,21,()=>Object.entries(A()),lt,(je,ce)=>{var ne=re(()=>Ma(a(ce),2));let ge=()=>a(ne)[0],Ue=()=>a(ne)[1];var Ee=Re(),te=xe(Ee);{var nt=et=>{l(et,()=>`${b()}.${ge()}`,ge,Ue)},pe=re(()=>E(Ue())),Te=et=>{o(et,()=>`${b()}.${ge()}`,ge,Ue)};K(te,et=>{a(pe)?et(nt):et(Te,-1)})}v(je,Ee)}),v(Ae,ke)},de=re(()=>E(A())),Pe=Ae=>{o(Ae,b,b,A)},Fe=re(()=>Array.isArray(A())),Ye=Ae=>{o(Ae,b,b,A)};K(z,Ae=>{a(de)?Ae(H):a(Fe)?Ae(Pe,1):Ae(Ye,-1)})}v(f,L)};let c=F(null),u=F(null),m=F(null),x=F(!0),w=F(!1),I=F(""),T=F(""),R=F(!1),N=F(!1),O=F(kt(new Set));const Z={provider:Nd,gateway:_d,channels:wd,agent:dd,memory:cd,security:Cd,heartbeat:xd,reliability:Ao,scheduler:pd,sessions_spawn:md,observability:fd,web_search:Ed,cost:hd,runtime:$d,tunnel:ud,identity:ld},q={provider:{label:"Provider 设置",defaultOpen:!0,fields:{api_key:{type:"string",sensitive:!0,label:"API Key",desc:"当前 Provider 的 API 密钥。修改后需要重启生效",default:""},api_url:{type:"string",label:"API URL",desc:"自定义 API 端点地址。留空使用 Provider 默认值（如 Ollama 填 http://localhost:11434）",default:""},default_provider:{type:"enum",label:"默认 Provider",desc:"选择 AI 模型提供商。决定使用哪个 API 来处理请求",default:"openrouter",options:["openrouter","anthropic","openai","ollama","gemini","groq","glm","xai","compatible","copilot","claude-cli","dashscope","dashscope-coding-intl","deepseek","fireworks","mistral","together"]},default_model:{type:"string",label:"默认模型",desc:"默认使用的模型名称（如 anthropic/claude-sonnet-4-6）",default:"anthropic/claude-sonnet-4.6"},default_temperature:{type:"number",label:"温度",desc:"模型输出的随机性（0=确定性，2=最随机）。推荐日常对话 0.7，代码任务 0.3",default:.7,min:0,max:2,step:.1}}},gateway:{label:"Gateway 网关",defaultOpen:!0,fields:{"gateway.port":{type:"number",label:"端口",desc:"Gateway HTTP 服务端口号",default:3e3,min:1,max:65535},"gateway.host":{type:"string",label:"监听地址",desc:"绑定的 IP 地址。127.0.0.1 仅本机访问，0.0.0.0 允许外部访问",default:"127.0.0.1"},"gateway.require_pairing":{type:"bool",label:"需要配对",desc:"开启后必须先配对才能访问 API。关闭则任何人可直接访问（不安全）",default:!0},"gateway.allow_public_bind":{type:"bool",label:"允许公网绑定",desc:"允许绑定到非 localhost 地址而不需要隧道。通常不建议开启",default:!1},"gateway.trust_forwarded_headers":{type:"bool",label:"信任代理头",desc:"信任 X-Forwarded-For / X-Real-IP 头。仅在反向代理后方启用",default:!1},"gateway.request_timeout_secs":{type:"number",label:"请求超时(秒)",desc:"HTTP 请求处理超时时间",default:60,min:5,max:600},"gateway.pair_rate_limit_per_minute":{type:"number",label:"配对速率限制(/分)",desc:"每客户端每分钟最大配对请求数",default:10,min:1,max:100},"gateway.webhook_rate_limit_per_minute":{type:"number",label:"Webhook 速率限制(/分)",desc:"每客户端每分钟最大 Webhook 请求数",default:60,min:1,max:1e3}}},channels:{label:"消息通道",defaultOpen:!0,fields:{"channels_config.message_timeout_secs":{type:"number",label:"消息处理超时(秒)",desc:"单条消息处理的最大超时时间（LLM + 工具调用）",default:300,min:30,max:3600},"channels_config.cli":{type:"bool",label:"CLI 交互模式",desc:"启用命令行交互通道",default:!0}}},agent:{label:"Agent 编排",defaultOpen:!1,fields:{"agent.max_tool_iterations":{type:"number",label:"最大工具循环次数",desc:"每条用户消息最多执行多少轮工具调用。设 0 回退到默认 10",default:10,min:0,max:100},"agent.max_history_messages":{type:"number",label:"最大历史消息数",desc:"每个会话保留的历史消息条数",default:50,min:5,max:500},"agent.parallel_tools":{type:"bool",label:"并行工具执行",desc:"允许在单次迭代中并行调用多个工具",default:!1},"agent.compact_context":{type:"bool",label:"紧凑上下文",desc:"为小模型（13B 以下）减少上下文大小",default:!1},"agent.compaction.mode":{type:"enum",label:"上下文压缩模式",desc:"off=不压缩，safeguard=保守压缩（默认），aggressive=激进截断",default:"safeguard",options:["off","safeguard","aggressive"]},"agent.compaction.max_context_tokens":{type:"number",label:"最大上下文 Token",desc:"触发压缩的 Token 阈值",default:128e3,min:1e3,max:1e6},"agent.compaction.keep_recent_messages":{type:"number",label:"压缩后保留消息数",desc:"压缩后保留最近的非系统消息数量",default:12,min:1,max:100},"agent.compaction.memory_flush":{type:"bool",label:"压缩前刷新记忆",desc:"在压缩之前提取并保存记忆",default:!0}}},memory:{label:"记忆存储",defaultOpen:!1,fields:{"memory.backend":{type:"enum",label:"存储后端",desc:"记忆存储引擎类型",default:"sqlite",options:["sqlite","postgres","markdown","lucid","none"]},"memory.auto_save":{type:"bool",label:"自动保存",desc:"自动保存用户输入到记忆",default:!0},"memory.hygiene_enabled":{type:"bool",label:"记忆清理",desc:"定期运行记忆归档和保留清理",default:!0},"memory.archive_after_days":{type:"number",label:"归档天数",desc:"超过此天数的日志/会话文件将被归档",default:7,min:1,max:365},"memory.purge_after_days":{type:"number",label:"清除天数",desc:"归档文件超过此天数后被清除",default:30,min:1,max:3650},"memory.conversation_retention_days":{type:"number",label:"对话保留天数",desc:"SQLite 后端：超过此天数的对话记录被清理",default:3,min:1,max:365},"memory.embedding_provider":{type:"enum",label:"嵌入提供商",desc:"记忆向量化的嵌入模型提供商",default:"none",options:["none","openai","custom"]},"memory.embedding_model":{type:"string",label:"嵌入模型",desc:"嵌入模型名称（如 text-embedding-3-small）",default:"text-embedding-3-small"},"memory.embedding_dimensions":{type:"number",label:"嵌入维度",desc:"嵌入向量的维度数",default:1536,min:64,max:4096},"memory.vector_weight":{type:"number",label:"向量权重",desc:"混合搜索中向量相似度的权重（0-1）",default:.7,min:0,max:1,step:.1},"memory.keyword_weight":{type:"number",label:"关键词权重",desc:"混合搜索中 BM25 关键词匹配的权重（0-1）",default:.3,min:0,max:1,step:.1},"memory.min_relevance_score":{type:"number",label:"最低相关性分数",desc:"低于此分数的记忆不会注入上下文",default:.4,min:0,max:1,step:.05},"memory.snapshot_enabled":{type:"bool",label:"记忆快照",desc:"定期将核心记忆导出为 MEMORY_SNAPSHOT.md",default:!1},"memory.auto_hydrate":{type:"bool",label:"自动恢复",desc:"当 brain.db 不存在时自动从快照恢复",default:!0}}},security:{label:"安全策略",defaultOpen:!1,fields:{"autonomy.level":{type:"enum",label:"自主级别",desc:"read_only=只读，supervised=需审批（默认），full=完全自主",default:"supervised",options:["read_only","supervised","full"]},"autonomy.workspace_only":{type:"bool",label:"仅工作区",desc:"限制文件写入和命令执行在工作区目录内",default:!0},"autonomy.max_actions_per_hour":{type:"number",label:"每小时最大操作数",desc:"每小时允许的最大操作次数",default:20,min:1,max:1e4},"autonomy.require_approval_for_medium_risk":{type:"bool",label:"中风险需审批",desc:"中等风险的 Shell 命令需要明确批准",default:!0},"autonomy.block_high_risk_commands":{type:"bool",label:"阻止高风险命令",desc:"即使在白名单中也阻止高风险命令",default:!0},"autonomy.allowed_commands":{type:"array",label:"允许的命令",desc:"允许执行的命令白名单",default:["git","npm","cargo","ls","cat","grep","find","echo"]},"secrets.encrypt":{type:"bool",label:"加密密钥",desc:"对 config.toml 中的 API Key 和 Token 进行加密存储",default:!0}}},heartbeat:{label:"心跳检测",defaultOpen:!1,fields:{"heartbeat.enabled":{type:"bool",label:"启用心跳",desc:"启用定期心跳检查",default:!1},"heartbeat.interval_minutes":{type:"number",label:"间隔(分钟)",desc:"心跳检查的时间间隔",default:30,min:1,max:1440},"heartbeat.active_hours":{type:"array",label:"活跃时段",desc:"心跳检查的有效小时范围（如 [8, 23]）",default:[8,23]},"heartbeat.prompt":{type:"string",label:"心跳提示词",desc:"心跳触发时使用的提示词",default:"Check HEARTBEAT.md and follow instructions."}}},reliability:{label:"可靠性",defaultOpen:!1,fields:{"reliability.provider_retries":{type:"number",label:"Provider 重试次数",desc:"调用 Provider 失败后的重试次数",default:2,min:0,max:10},"reliability.provider_backoff_ms":{type:"number",label:"重试退避(ms)",desc:"Provider 重试的基础退避时间",default:500,min:100,max:3e4},"reliability.fallback_providers":{type:"array",label:"备用 Provider",desc:"主 Provider 不可用时按顺序尝试的备用列表",default:[]},"reliability.api_keys":{type:"array",label:"轮换 API Key",desc:"遇到速率限制时轮换使用的额外 API Key",default:[]},"reliability.channel_initial_backoff_secs":{type:"number",label:"通道初始退避(秒)",desc:"通道/守护进程重启的初始退避时间",default:2,min:1,max:60},"reliability.channel_max_backoff_secs":{type:"number",label:"通道最大退避(秒)",desc:"通道/守护进程重启的最大退避时间",default:60,min:5,max:3600}}},scheduler:{label:"调度器",defaultOpen:!1,fields:{"scheduler.enabled":{type:"bool",label:"启用调度器",desc:"启用内置定时任务调度循环",default:!0},"scheduler.max_tasks":{type:"number",label:"最大任务数",desc:"最多持久化保存的计划任务数量",default:64,min:1,max:1e3},"scheduler.max_concurrent":{type:"number",label:"最大并发数",desc:"每次调度周期内最多执行的任务数",default:4,min:1,max:32},"cron.enabled":{type:"bool",label:"启用 Cron",desc:"启用 Cron 子系统",default:!0},"cron.max_run_history":{type:"number",label:"Cron 历史记录数",desc:"保留的 Cron 运行历史记录条数",default:50,min:10,max:1e3}}},sessions_spawn:{label:"子进程管理",defaultOpen:!1,fields:{"sessions_spawn.default_mode":{type:"enum",label:"默认模式",desc:"子进程默认执行模式",default:"task",options:["task","process"]},"sessions_spawn.max_concurrent":{type:"number",label:"最大并发数",desc:"全局最大并发子进程/任务数",default:4,min:1,max:32},"sessions_spawn.max_spawn_depth":{type:"number",label:"最大嵌套深度",desc:"子进程可以再次 spawn 的最大深度",default:2,min:1,max:10},"sessions_spawn.max_children_per_agent":{type:"number",label:"每父进程最大子数",desc:"每个父会话允许的最大并发子运行数",default:5,min:1,max:20},"sessions_spawn.cleanup_on_complete":{type:"bool",label:"完成后清理",desc:"进程模式完成后删除工作区目录",default:!0}}},observability:{label:"可观测性",defaultOpen:!1,fields:{"observability.backend":{type:"enum",label:"后端",desc:"可观测性后端类型",default:"none",options:["none","log","prometheus","otel"]},"observability.otel_endpoint":{type:"string",label:"OTLP 端点",desc:"OpenTelemetry Collector 端点 URL（仅 otel 后端）",default:""},"observability.otel_service_name":{type:"string",label:"服务名称",desc:"上报给 OTel 的服务名称",default:"openprx"}}},web_search:{label:"网络搜索",defaultOpen:!1,fields:{"web_search.enabled":{type:"bool",label:"启用搜索",desc:"启用网络搜索工具",default:!1},"web_search.provider":{type:"enum",label:"搜索引擎",desc:"搜索提供商。DuckDuckGo 免费无 Key，Brave 需要 API Key",default:"duckduckgo",options:["duckduckgo","brave"]},"web_search.brave_api_key":{type:"string",sensitive:!0,label:"Brave API Key",desc:"Brave Search API 密钥（选 Brave 时必填）",default:""},"web_search.max_results":{type:"number",label:"最大结果数",desc:"每次搜索返回的最大结果数（1-10）",default:5,min:1,max:10},"web_search.fetch_enabled":{type:"bool",label:"启用页面抓取",desc:"允许抓取和提取网页可读内容",default:!0},"web_search.fetch_max_chars":{type:"number",label:"抓取最大字符",desc:"网页抓取返回的最大字符数",default:1e4,min:100,max:1e5}}},cost:{label:"成本控制",defaultOpen:!1,fields:{"cost.enabled":{type:"bool",label:"启用成本追踪",desc:"启用 API 调用成本追踪和预算控制",default:!1},"cost.daily_limit_usd":{type:"number",label:"日限额(USD)",desc:"每日消费上限（美元）",default:10,min:.1,max:1e4,step:.1},"cost.monthly_limit_usd":{type:"number",label:"月限额(USD)",desc:"每月消费上限（美元）",default:100,min:1,max:1e5,step:1},"cost.warn_at_percent":{type:"number",label:"预警百分比",desc:"消费达到限额的多少百分比时发出警告",default:80,min:10,max:100}}},runtime:{label:"运行时",defaultOpen:!1,fields:{"runtime.kind":{type:"enum",label:"运行时类型",desc:"命令执行环境：native=本机，docker=容器隔离",default:"native",options:["native","docker"]},"runtime.reasoning_enabled":{type:"enum",label:"推理模式",desc:"全局推理/思考模式：null=Provider 默认，true=启用，false=禁用",default:"",options:["","true","false"]}}},tunnel:{label:"隧道",defaultOpen:!1,fields:{"tunnel.provider":{type:"enum",label:"隧道类型",desc:"将 Gateway 暴露到公网的隧道服务",default:"none",options:["none","cloudflare","tailscale","ngrok","custom"]}}},identity:{label:"身份格式",defaultOpen:!1,fields:{"identity.format":{type:"enum",label:"身份格式",desc:"OpenClaw 或 AIEOS 身份文档格式",default:"openclaw",options:["openclaw","aieos"]}}}};function E(f){return f!==null&&typeof f=="object"&&!Array.isArray(f)}function h(f){return typeof f=="boolean"?"bool":typeof f=="number"?"number":Array.isArray(f)?"array":E(f)?"object":"string"}function M(f){const b=String(f).toLowerCase();return["key","token","secret","password","auth","credential","private"].some(A=>b.includes(A))}function C(f){return String(f).replace(/_/g," ").replace(/\b\w/g,b=>b.toUpperCase())}function U(){const f=new Set;for(const b of Object.values(q))for(const A of Object.keys(b.fields))f.add(A.split(".")[0]);return f}const ie=U();function fe(f){if(!a(c))return[];const b=new Set(Object.keys(f.fields)),A=new Set;for(const z of Object.keys(f.fields))A.add(z.split(".")[0]);const L=[];for(const z of A){const H=a(c)[z];if(E(H))for(const[de,Pe]of Object.entries(H)){const Fe=`${z}.${de}`;b.has(Fe)||L.push({path:Fe,key:de,value:Pe})}}return L}const We=re(()=>a(c)?Object.keys(a(c)).filter(f=>!ie.has(f)).sort():[]);function Me(f,b){if(!f)return;const A=b.split(".");let L=f;for(const z of A){if(L==null||typeof L!="object")return;L=L[z]}return L}function W(f,b,A){const L=b.split(".");let z=f;for(let H=0;H<L.length-1;H++)(z[L[H]]==null||typeof z[L[H]]!="object")&&(z[L[H]]={}),z=z[L[H]];z[L[L.length-1]]=A}function Y(f){if(a(c))return Me(a(c),f)}function be(f){return JSON.parse(JSON.stringify(f))}function ae(f,b){return JSON.stringify(f)===JSON.stringify(b)}function ze(f,b,A){const L=[],z=new Set([...Object.keys(f||{}),...Object.keys(b||{})]);for(const H of z){const de=A?`${A}.${H}`:H,Pe=(f||{})[H],Fe=(b||{})[H];E(Pe)&&E(Fe)?L.push(...ze(Pe,Fe,de)):ae(Pe,Fe)||L.push({fieldPath:de,newVal:Pe,oldVal:Fe})}return L}function B(){return!a(c)||!a(u)?[]:ze(a(c),a(u),"").map(b=>{for(const L of Object.values(q))if(L.fields[b.fieldPath])return{...b,label:L.fields[b.fieldPath].label,group:L.label};const A=b.fieldPath.split(".");return{...b,label:C(A[A.length-1]),group:C(A[0])}})}const V=re(()=>!!(a(c)&&a(u)&&JSON.stringify(a(c))!==JSON.stringify(a(u)))),ve=re(B),Se=re(()=>new Set(a(ve).map(f=>f.fieldPath)));function ee(f){for(const b of a(Se))if(b===f||b.startsWith(f+"."))return!0;return!1}const X=re(()=>a(c)?JSON.stringify(a(c),null,2):""),ue=re(()=>Ec(a(X)));function oe(f,b){if(!a(c))return;const A=be(a(c));W(A,f,b),y(c,A,!0)}function rt(f){const b=Y(f),A=Array.isArray(b)?[...b,""]:[""];oe(f,A)}function it(f,b){const A=Y(f);Array.isArray(A)&&oe(f,A.filter((L,z)=>z!==b))}function Qe(f,b,A){const L=Y(f);if(!Array.isArray(L))return;const z=[...L];z[b]=A,oe(f,z)}function dt(f){const b=new Set(a(O));b.has(f)?b.delete(f):b.add(f),y(O,b,!0)}function j(f){return f==null?"null":typeof f=="boolean"?f?"true":"false":Array.isArray(f)||typeof f=="object"?JSON.stringify(f):String(f)}function Q(f){return!!(E(f)||Array.isArray(f)&&f.some(b=>E(b)||Array.isArray(b)))}async function ye(){try{const[f,b]=await Promise.all([wt.getConfig(),wt.getStatus().catch(()=>null)]);y(c,typeof f=="object"&&f?f:{},!0),y(u,be(a(c)),!0),y(m,b,!0),y(I,"")}catch(f){y(I,f instanceof Error?f.message:"Failed to load config",!0)}finally{y(x,!1)}}async function st(){if(!(!a(V)||a(w))){y(w,!0),y(T,"");try{const f={};for(const A of a(ve))W(f,A.fieldPath,A.newVal);const b=await wt.saveConfig(f);y(u,be(a(c)),!0),y(N,!1),b!=null&&b.restart_required?y(T,"已保存，部分设置需要重启服务后生效"):y(T,"已保存"),setTimeout(()=>{y(T,"")},5e3)}catch(f){y(T,"保存失败: "+(f instanceof Error?f.message:String(f)))}finally{y(w,!1)}}}function Ze(){a(u)&&(y(c,be(a(u)),!0),y(N,!1))}async function ot(){if(!(!a(X)||typeof navigator>"u"||!navigator.clipboard))try{await navigator.clipboard.writeText(a(X))}catch{}}Ut(()=>{ye()});var bt=vu(),Ie=i(bt),Ve=i(Ie),ct=i(Ve),He=g(Ve,2),yt=i(He),le=i(yt),Le=g(yt,2),he=g(Ie,2);{var Ne=f=>{var b=Zc();v(f,b)},D=f=>{var b=eu(),A=i(b);P(()=>p(A,a(I))),v(f,b)},G=f=>{var b=tu(),A=i(b),L=i(A),z=i(L);nl(z,()=>a(ue)),v(f,b)},qe=f=>{var b=lu(),A=i(b);at(A,17,()=>Object.entries(q),lt,(H,de)=>{var Pe=re(()=>Ma(a(de),2));let Fe=()=>a(Pe)[0],Ye=()=>a(Pe)[1];const Ae=re(()=>Z[Fe()]),ke=re(()=>fe(Ye())),je=re(()=>Object.keys(Ye().fields)),ce=re(()=>a(je).some(Ke=>a(Se).has(Ke))||a(ke).some(Ke=>ee(Ke.path)));var ne=nu(),ge=i(ne),Ue=i(ge);{var Ee=Ke=>{var _t=Re(),St=xe(_t);sl(St,()=>a(Ae),(Lt,Ft)=>{Ft(Lt,{size:18,class:"text-gray-500 dark:text-gray-400"})}),v(Ke,_t)};K(Ue,Ke=>{a(Ae)&&Ke(Ee)})}var te=g(Ue,2),nt=i(te),pe=g(te,2);{var Te=Ke=>{var _t=ru();v(Ke,_t)};K(pe,Ke=>{a(ce)&&Ke(Te)})}var et=g(ge,2),we=i(et);at(we,17,()=>Object.entries(Ye().fields),lt,(Ke,_t)=>{var St=re(()=>Ma(a(_t),2));r(Ke,()=>a(St)[0],()=>a(St)[1])});var se=g(we,2);{var Xe=Ke=>{var _t=au(),St=g(i(_t),2);at(St,21,()=>a(ke),lt,(Lt,Ft)=>{let Qr=()=>a(Ft).path,Qt=()=>a(Ft).key,ka=()=>a(Ft).value;var wa=Re(),Ua=xe(wa);{var Sa=Zr=>{l(Zr,Qr,Qt,ka)},dn=re(()=>E(ka())),cn=Zr=>{o(Zr,Qr,Qt,ka)};K(Ua,Zr=>{a(dn)?Zr(Sa):Zr(cn,-1)})}v(Lt,wa)}),v(Ke,_t)};K(se,Ke=>{a(ke).length>0&&Ke(Xe)})}P(()=>{ne.open=Ye().defaultOpen,p(nt,Ye().label)}),v(H,ne)});var L=g(A,2);{var z=H=>{var de=iu(),Pe=g(i(de),2);at(Pe,21,()=>a(We),lt,(Fe,Ye)=>{const Ae=re(()=>a(c)[a(Ye)]),ke=re(()=>ee(a(Ye))),je=re(()=>h(a(Ae)));var ce=ou(),ne=i(ce),ge=i(ne);yd(ge,{size:18,class:"flex-shrink-0 text-gray-400 dark:text-gray-500"});var Ue=g(ge,2),Ee=i(Ue),te=g(Ue,2);{var nt=se=>{var Xe=su();v(se,Xe)};K(te,se=>{a(ke)&&se(nt)})}var pe=g(te,2),Te=i(pe),et=g(ne,2),we=i(et);d(we,()=>a(Ye),()=>a(Ae)),P(()=>{p(Ee,a(Ye)),p(Te,a(je))}),v(Fe,ce)}),v(H,de)};K(L,H=>{a(We).length>0&&H(z)})}v(f,b)};K(he,f=>{a(x)?f(Ne):a(I)?f(D,1):a(R)?f(G,2):f(qe,-1)})}var Je=g(he,2);{var ut=f=>{var b=uu(),A=i(b),L=i(A),z=i(L),H=i(z),de=g(z,2),Pe=i(de),Fe=g(L,2),Ye=i(Fe),Ae=g(Ye,2),ke=i(Ae),je=g(A,2);{var ce=ne=>{var ge=cu(),Ue=g(i(ge),2);at(Ue,21,()=>a(ve),lt,(Ee,te)=>{var nt=du(),pe=i(nt),Te=i(pe),et=g(pe,2),we=i(et),se=g(et,2),Xe=i(se),Ke=g(se,4),_t=i(Ke);P((St,Lt)=>{p(Te,a(te).group),p(we,a(te).label),p(Xe,St),p(_t,Lt)},[()=>j(a(te).oldVal),()=>j(a(te).newVal)]),v(Ee,nt)}),v(ne,ge)};K(je,ne=>{a(N)&&ne(ce)})}P(()=>{p(H,`${a(ve).length??""} 项更改`),p(Pe,a(N)?"隐藏详情":"查看详情"),Ae.disabled=a(w),p(ke,a(w)?"保存中...":"保存配置")}),J("click",de,()=>y(N,!a(N))),J("click",Ye,Ze),J("click",Ae,st),v(f,b)};K(Je,f=>{a(V)&&!a(x)&&!a(R)&&f(ut)})}var mt=g(Je,2);{var S=f=>{var b=fu(),A=i(b);P(L=>{tt(b,1,`fixed bottom-20 left-1/2 z-50 -translate-x-1/2 rounded-lg border px-4 py-2 text-sm shadow-lg ${L??""}`),p(A,a(T))},[()=>a(T).startsWith("保存失败")?"border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300":"border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300"]),v(f,b)};K(mt,f=>{a(T)&&f(S)})}P(f=>{p(ct,f),p(le,a(R)?"结构化编辑":"JSON 视图")},[()=>_("config.title")]),J("click",yt,()=>y(R,!a(R))),J("click",Le,ot),v(e,bt),Ce()}or(["click","change","input"]);var pu=k('<p class="text-gray-400 dark:text-gray-500"> </p>'),bu=k('<li class="whitespace-pre-wrap break-words"><span class="mr-3 select-none text-gray-400 dark:text-gray-600"> </span> <span> </span></li>'),yu=k('<ol class="space-y-1"></ol>'),hu=k('<section class="space-y-4"><div class="flex flex-wrap items-center justify-between gap-3"><h2 class="text-2xl font-semibold"> </h2> <div class="flex items-center gap-2"><span> </span> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></div> <div class="h-[65vh] overflow-y-auto rounded-xl border border-gray-200 bg-gray-50 p-4 font-mono text-xs leading-5 text-green-800 dark:border-gray-700 dark:bg-gray-950 dark:text-green-300"><!></div></section>');function mu(e,t){$e(t,!0);const r=1e3,n=500,s=1e4;let o=F(kt([])),l=F(!1),d=F("disconnected"),c=F(null),u=null,m=null,x=0,w=!0;const I=re(()=>a(d)==="connected"?"border-green-500/50 bg-green-500/15 text-green-700 dark:text-green-300":a(d)==="reconnecting"?"border-amber-500/50 bg-amber-500/15 text-amber-700 dark:text-amber-200":"border-red-500/50 bg-red-500/15 text-red-700 dark:text-red-300"),T=re(()=>a(d)==="connected"?_("logs.connected"):a(d)==="reconnecting"?_("logs.reconnecting"):_("logs.disconnected"));function R(ee){const X=Ya?new URL(Ya,window.location.href):new URL(window.location.href);return X.protocol=X.protocol==="https:"?"wss:":"ws:",X.pathname="/api/logs/stream",X.search=`token=${encodeURIComponent(ee)}`,X.hash="",X.toString()}function N(ee){if(typeof ee!="string"||ee.length===0)return;const X=ee.split(/\r?\n/).filter(oe=>oe.length>0);if(X.length===0)return;const ue=[...a(o),...X];y(o,ue.length>r?ue.slice(ue.length-r):ue,!0)}function O(){m!==null&&(clearTimeout(m),m=null)}function Z(){u&&(u.onopen=null,u.onmessage=null,u.onerror=null,u.onclose=null,u.close(),u=null)}function q(){if(!w){y(d,"disconnected");return}y(d,"reconnecting");const ee=Math.min(n*2**x,s);x+=1,O(),m=setTimeout(()=>{m=null,E()},ee)}function E(){O();const ee=Fa();if(!ee){y(d,"disconnected");return}y(d,"reconnecting"),Z();let X;try{X=new WebSocket(R(ee))}catch{q();return}u=X,X.onopen=()=>{x=0,y(d,"connected")},X.onmessage=ue=>{a(l)||N(ue.data)},X.onerror=()=>{(X.readyState===WebSocket.OPEN||X.readyState===WebSocket.CONNECTING)&&X.close()},X.onclose=()=>{u=null,q()}}function h(){y(l,!a(l))}function M(){y(o,[],!0)}Ut(()=>(w=!0,E(),()=>{w=!1,O(),Z(),y(d,"disconnected")})),Ut(()=>{a(o).length,a(l),!(a(l)||!a(c))&&queueMicrotask(()=>{a(c)&&(a(c).scrollTop=a(c).scrollHeight)})});var C=hu(),U=i(C),ie=i(U),fe=i(ie),We=g(ie,2),Me=i(We),W=i(Me),Y=g(Me,2),be=i(Y),ae=g(Y,2),ze=i(ae),B=g(U,2),V=i(B);{var ve=ee=>{var X=pu(),ue=i(X);P(oe=>p(ue,oe),[()=>_("logs.waiting")]),v(ee,X)},Se=ee=>{var X=yu();at(X,21,()=>a(o),lt,(ue,oe,rt)=>{var it=bu(),Qe=i(it),dt=i(Qe),j=g(Qe,2),Q=i(j);P(ye=>{p(dt,ye),p(Q,a(oe))},[()=>String(rt+1).padStart(4,"0")]),v(ue,it)}),v(ee,X)};K(V,ee=>{a(o).length===0?ee(ve):ee(Se,-1)})}Ln(B,ee=>y(c,ee),()=>a(c)),P((ee,X,ue)=>{p(fe,ee),tt(Me,1,`rounded-full border px-2 py-1 text-xs font-medium uppercase tracking-wide ${a(I)}`),p(W,a(T)),p(be,X),p(ze,ue)},[()=>_("logs.title"),()=>a(l)?_("logs.resume"):_("logs.pause"),()=>_("logs.clear")]),J("click",Y,h),J("click",ae,M),v(e,C),Ce()}or(["click"]);var _u=k("<option> </option>"),xu=k('<div class="rounded-xl border border-sky-500/30 bg-white p-4 space-y-3 dark:bg-gray-800"><h3 class="text-base font-semibold text-gray-900 dark:text-gray-100"> </h3> <div class="grid gap-3 sm:grid-cols-2"><div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select></div> <div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="number" min="1000" step="1000" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="sm:col-span-2"><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="flex items-center gap-2"><label class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <button type="button"><span></span></button></div></div> <div class="flex justify-end gap-2 pt-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div></div>'),ku=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),wu=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Su=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Au=k("<option> </option>"),Eu=k('<div class="space-y-3"><div class="grid gap-3 sm:grid-cols-2"><div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select></div> <div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="number" min="1000" step="1000" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="sm:col-span-2"><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="flex items-center gap-2"><label class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <button type="button"><span></span></button></div></div> <div class="flex justify-end gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div></div>'),$u=k('<div class="flex items-start justify-between gap-3"><div class="min-w-0 flex-1"><div class="flex items-center gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-2 font-mono text-sm text-gray-500 dark:text-gray-400"> </p> <p class="mt-1 text-xs text-gray-400 dark:text-gray-500"> </p></div> <div class="flex items-center gap-2"><button type="button"><span></span></button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-2 py-1 text-xs text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-red-500/50 bg-red-500/10 px-2 py-1 text-xs text-red-600 hover:bg-red-500/20 dark:text-red-300"> </button></div></div>'),Cu=k('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><!></article>'),Mu=k('<div class="space-y-3"></div>'),Nu=k('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div> <!> <!></section>');function Pu(e,t){$e(t,!0);const r=["agent_start","agent_end","llm_request","llm_response","tool_call_start","tool_call_end","message_received","message_sent"];let n=F(kt([])),s=F(!0),o=F(""),l=F(null),d=F(!1),c=F(kt(r[0])),u=F(""),m=F(3e4),x=F(!0);function w(){y(c,r[0],!0),y(u,""),y(m,3e4),y(x,!0)}function I(B){return B.split("_").map(V=>V.charAt(0).toUpperCase()+V.slice(1)).join(" ")}async function T(){try{const B=await wt.getHooks();y(n,Array.isArray(B==null?void 0:B.hooks)?B.hooks:[],!0),y(o,"")}catch{y(n,[{id:"1",event:"message_received",command:'echo "msg received"',timeout_ms:3e4,enabled:!0},{id:"2",event:"agent_start",command:"/opt/scripts/on-start.sh",timeout_ms:1e4,enabled:!0},{id:"3",event:"tool_call_end",command:'notify-send "tool done"',timeout_ms:5e3,enabled:!1}],!0),y(o,"")}finally{y(s,!1)}}function R(B){y(l,B.id,!0),y(c,B.event,!0),y(u,B.command,!0),y(m,B.timeout_ms,!0),y(x,B.enabled,!0)}function N(){y(l,null),w()}function O(B){y(n,a(n).map(V=>V.id===B?{...V,event:a(c),command:a(u),timeout_ms:a(m),enabled:a(x)}:V),!0),y(l,null),w()}function Z(){if(!a(u).trim())return;const B={id:String(Date.now()),event:a(c),command:a(u).trim(),timeout_ms:a(m),enabled:a(x)};y(n,[...a(n),B],!0),y(d,!1),w()}function q(B){y(n,a(n).filter(V=>V.id!==B),!0)}function E(B){y(n,a(n).map(V=>V.id===B?{...V,enabled:!V.enabled}:V),!0)}Ut(()=>{T()});var h=Nu(),M=i(h),C=i(M),U=i(C),ie=g(C,2),fe=i(ie),We=g(M,2);{var Me=B=>{var V=xu(),ve=i(V),Se=i(ve),ee=g(ve,2),X=i(ee),ue=i(X),oe=i(ue),rt=g(ue,2);at(rt,21,()=>r,lt,(Ne,D)=>{var G=_u(),qe=i(G),Je={};P(ut=>{p(qe,ut),Je!==(Je=a(D))&&(G.value=(G.__value=a(D))??"")},[()=>I(a(D))]),v(Ne,G)});var it=g(X,2),Qe=i(it),dt=i(Qe),j=g(Qe,2),Q=g(it,2),ye=i(Q),st=i(ye),Ze=g(ye,2),ot=g(Q,2),bt=i(ot),Ie=i(bt),Ve=g(bt,2),ct=i(Ve),He=g(ee,2),yt=i(He),le=i(yt),Le=g(yt,2),he=i(Le);P((Ne,D,G,qe,Je,ut,mt,S)=>{p(Se,Ne),p(oe,D),p(dt,G),p(st,qe),ht(Ze,"placeholder",Je),p(Ie,ut),tt(Ve,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a(x)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),tt(ct,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(x)?"translate-x-4":"translate-x-1"}`),p(le,mt),p(he,S)},[()=>_("hooks.newHook"),()=>_("hooks.event"),()=>_("hooks.timeout"),()=>_("hooks.command"),()=>_("hooks.commandPlaceholder"),()=>_("hooks.enabled"),()=>_("hooks.cancel"),()=>_("hooks.save")]),In(rt,()=>a(c),Ne=>y(c,Ne)),Hr(j,()=>a(m),Ne=>y(m,Ne)),Hr(Ze,()=>a(u),Ne=>y(u,Ne)),J("click",Ve,()=>y(x,!a(x))),J("click",yt,()=>{y(d,!1),w()}),J("click",Le,Z),v(B,V)};K(We,B=>{a(d)&&B(Me)})}var W=g(We,2);{var Y=B=>{var V=ku(),ve=i(V);P(Se=>p(ve,Se),[()=>_("hooks.loading")]),v(B,V)},be=B=>{var V=wu(),ve=i(V);P(()=>p(ve,a(o))),v(B,V)},ae=B=>{var V=Su(),ve=i(V);P(Se=>p(ve,Se),[()=>_("hooks.noHooks")]),v(B,V)},ze=B=>{var V=Mu();at(V,21,()=>a(n),ve=>ve.id,(ve,Se)=>{var ee=Cu(),X=i(ee);{var ue=rt=>{var it=Eu(),Qe=i(it),dt=i(Qe),j=i(dt),Q=i(j),ye=g(j,2);at(ye,21,()=>r,lt,(mt,S)=>{var f=Au(),b=i(f),A={};P(L=>{p(b,L),A!==(A=a(S))&&(f.value=(f.__value=a(S))??"")},[()=>I(a(S))]),v(mt,f)});var st=g(dt,2),Ze=i(st),ot=i(Ze),bt=g(Ze,2),Ie=g(st,2),Ve=i(Ie),ct=i(Ve),He=g(Ve,2),yt=g(Ie,2),le=i(yt),Le=i(le),he=g(le,2),Ne=i(he),D=g(Qe,2),G=i(D),qe=i(G),Je=g(G,2),ut=i(Je);P((mt,S,f,b,A,L)=>{p(Q,mt),p(ot,S),p(ct,f),p(Le,b),tt(he,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a(x)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),tt(Ne,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(x)?"translate-x-4":"translate-x-1"}`),p(qe,A),p(ut,L)},[()=>_("hooks.event"),()=>_("hooks.timeout"),()=>_("hooks.command"),()=>_("hooks.enabled"),()=>_("hooks.cancel"),()=>_("hooks.save")]),In(ye,()=>a(c),mt=>y(c,mt)),Hr(bt,()=>a(m),mt=>y(m,mt)),Hr(He,()=>a(u),mt=>y(u,mt)),J("click",he,()=>y(x,!a(x))),J("click",G,N),J("click",Je,()=>O(a(Se).id)),v(rt,it)},oe=rt=>{var it=$u(),Qe=i(it),dt=i(Qe),j=i(dt),Q=i(j),ye=g(j,2),st=i(ye),Ze=g(dt,2),ot=i(Ze),bt=g(Ze,2),Ie=i(bt),Ve=g(Qe,2),ct=i(Ve),He=i(ct),yt=g(ct,2),le=i(yt),Le=g(yt,2),he=i(Le);P((Ne,D,G,qe,Je)=>{p(Q,Ne),tt(ye,1,`rounded-full px-2 py-1 text-xs font-medium ${a(Se).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),p(st,D),p(ot,a(Se).command),p(Ie,`${G??""}: ${a(Se).timeout_ms??""}ms`),tt(ct,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a(Se).enabled?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),tt(He,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(Se).enabled?"translate-x-4":"translate-x-1"}`),p(le,qe),p(he,Je)},[()=>I(a(Se).event),()=>a(Se).enabled?_("common.enabled"):_("common.disabled"),()=>_("hooks.timeout"),()=>_("hooks.edit"),()=>_("hooks.delete")]),J("click",ct,()=>E(a(Se).id)),J("click",yt,()=>R(a(Se))),J("click",Le,()=>q(a(Se).id)),v(rt,it)};K(X,rt=>{a(l)===a(Se).id?rt(ue):rt(oe,-1)})}v(ve,ee)}),v(B,V)};K(W,B=>{a(s)?B(Y):a(o)?B(be,1):a(n).length===0?B(ae,2):B(ze,-1)})}P((B,V)=>{p(U,B),p(fe,V)},[()=>_("hooks.title"),()=>a(d)?_("hooks.cancelAdd"):_("hooks.addHook")]),J("click",ie,()=>{y(d,!a(d)),a(d)&&w()}),v(e,h),Ce()}or(["click"]);var Tu=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Ou=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Iu=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Lu=k('<p class="mt-1 text-xs text-gray-500 dark:text-gray-400"> </p>'),Fu=k('<div class="rounded-lg border border-gray-200 bg-gray-50/60 p-3 dark:border-gray-700 dark:bg-gray-900/60"><p class="font-mono text-sm font-medium text-gray-700 dark:text-gray-200"> </p> <!></div>'),Ru=k('<div class="border-t border-gray-200 p-4 dark:border-gray-700"><h4 class="mb-3 text-sm font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </h4> <div class="grid gap-2"></div></div>'),ju=k('<div class="border-t border-gray-200 p-4 dark:border-gray-700"><p class="text-sm text-gray-500 dark:text-gray-400"> </p></div>'),Du=k('<article class="rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><button type="button" class="flex w-full items-center justify-between gap-3 p-4 text-left"><div class="min-w-0 flex-1"><div class="flex items-center gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-1 font-mono text-sm text-gray-500 dark:text-gray-400"> </p></div> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></button> <!></article>'),zu=k('<div class="space-y-4"></div>'),Hu=k('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!></section>');function Uu(e,t){$e(t,!0);let r=F(kt([])),n=F(!0),s=F(""),o=F(null);async function l(){try{const E=await wt.getMcpServers();y(r,Array.isArray(E==null?void 0:E.servers)?E.servers:[],!0),y(s,"")}catch{y(r,[{name:"filesystem",url:"stdio:///usr/local/bin/mcp-filesystem",status:"connected",tools:[{name:"read_file",description:"Read contents of a file"},{name:"write_file",description:"Write content to a file"},{name:"list_directory",description:"List directory contents"}]},{name:"github",url:"https://mcp.github.com/sse",status:"connected",tools:[{name:"search_repositories",description:"Search GitHub repositories"},{name:"create_issue",description:"Create a new issue"},{name:"list_pull_requests",description:"List pull requests"}]},{name:"database",url:"stdio:///opt/mcp/db-server",status:"disconnected",tools:[]}],!0),y(s,"")}finally{y(n,!1)}}function d(E){y(o,a(o)===E?null:E,!0)}async function c(){y(n,!0),await l()}Ut(()=>{l()});var u=Hu(),m=i(u),x=i(m),w=i(x),I=g(x,2),T=i(I),R=g(m,2);{var N=E=>{var h=Tu(),M=i(h);P(C=>p(M,C),[()=>_("mcp.loading")]),v(E,h)},O=E=>{var h=Ou(),M=i(h);P(()=>p(M,a(s))),v(E,h)},Z=E=>{var h=Iu(),M=i(h);P(C=>p(M,C),[()=>_("mcp.noServers")]),v(E,h)},q=E=>{var h=zu();at(h,21,()=>a(r),lt,(M,C)=>{var U=Du(),ie=i(U),fe=i(ie),We=i(fe),Me=i(We),W=i(Me),Y=g(Me,2),be=i(Y),ae=g(We,2),ze=i(ae),B=g(fe,2),V=i(B),ve=g(ie,2);{var Se=X=>{var ue=Ru(),oe=i(ue),rt=i(oe),it=g(oe,2);at(it,21,()=>a(C).tools,lt,(Qe,dt)=>{var j=Fu(),Q=i(j),ye=i(Q),st=g(Q,2);{var Ze=ot=>{var bt=Lu(),Ie=i(bt);P(()=>p(Ie,a(dt).description)),v(ot,bt)};K(st,ot=>{a(dt).description&&ot(Ze)})}P(()=>p(ye,a(dt).name)),v(Qe,j)}),P(Qe=>p(rt,Qe),[()=>_("mcp.availableTools")]),v(X,ue)},ee=X=>{var ue=ju(),oe=i(ue),rt=i(oe);P(it=>p(rt,it),[()=>_("mcp.noTools")]),v(X,ue)};K(ve,X=>{a(o)===a(C).name&&a(C).tools&&a(C).tools.length>0?X(Se):a(o)===a(C).name&&(!a(C).tools||a(C).tools.length===0)&&X(ee,1)})}P((X,ue)=>{var oe;p(W,a(C).name),tt(Y,1,`rounded-full px-2 py-1 text-xs font-medium ${a(C).status==="connected"?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),p(be,X),p(ze,a(C).url),p(V,`${((oe=a(C).tools)==null?void 0:oe.length)??0??""} ${ue??""}`)},[()=>a(C).status==="connected"?_("mcp.connected"):_("mcp.disconnected"),()=>_("mcp.tools")]),J("click",ie,()=>d(a(C).name)),v(M,U)}),v(E,h)};K(R,E=>{a(n)?E(N):a(s)?E(O,1):a(r).length===0?E(Z,2):E(q,-1)})}P((E,h)=>{p(w,E),p(T,h)},[()=>_("mcp.title"),()=>_("common.refresh")]),J("click",I,c),v(e,u),Ce()}or(["click"]);var Bu=k('<span class="text-sm text-gray-500 dark:text-gray-400"> </span>'),Wu=k("<div> </div>"),Vu=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),qu=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Ku=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Gu=k('<p class="mt-2 text-sm text-gray-500 dark:text-gray-400"> </p>'),Ju=k('<div class="flex items-center gap-2"><span class="text-xs text-yellow-600 dark:text-yellow-400"> </span> <button type="button" class="rounded px-2 py-1 text-xs font-medium text-red-500 transition hover:bg-red-500/20 disabled:opacity-50 dark:text-red-400"> </button> <button type="button" class="rounded px-2 py-1 text-xs text-gray-500 transition hover:bg-gray-200 dark:text-gray-400 dark:hover:bg-gray-700"> </button></div>'),Yu=k('<button type="button" class="rounded px-2 py-1 text-xs text-red-500 transition hover:bg-red-500/20 dark:text-red-400"> </button>'),Xu=k('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <button type="button"><span></span></button></div> <!> <p class="mt-2 font-mono text-xs text-gray-400 dark:text-gray-500"> </p> <div class="mt-3 flex items-center justify-between"><span> </span> <!></div></article>'),Qu=k('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),Zu=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),e0=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),t0=k('<p class="mt-2 line-clamp-3 text-sm text-gray-500 dark:text-gray-400"> </p>'),r0=k('<span class="flex items-center gap-1"><svg class="h-3.5 w-3.5" fill="currentColor" viewBox="0 0 20 20"><path d="M9.049 2.927c.3-.921 1.603-.921 1.902 0l1.07 3.292a1 1 0 00.95.69h3.462c.969 0 1.371 1.24.588 1.81l-2.8 2.034a1 1 0 00-.364 1.118l1.07 3.292c.3.921-.755 1.688-1.54 1.118l-2.8-2.034a1 1 0 00-1.175 0l-2.8 2.034c-.784.57-1.838-.197-1.539-1.118l1.07-3.292a1 1 0 00-.364-1.118L2.98 8.72c-.783-.57-.38-1.81.588-1.81h3.461a1 1 0 00.951-.69l1.07-3.292z"></path></svg> </span>'),a0=k('<span class="rounded bg-gray-100 px-1.5 py-0.5 dark:bg-gray-700"> </span>'),n0=k('<span class="rounded-full border border-green-500/50 bg-green-500/20 px-2 py-1 text-xs font-medium text-green-700 dark:text-green-300"> </span>'),s0=k('<button type="button" class="rounded-lg bg-sky-600 px-3 py-1 text-xs font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"> </button>'),o0=k('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-2"><div class="min-w-0 flex-1"><h3 class="truncate text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <p class="text-xs text-gray-400 dark:text-gray-500"> </p></div> <span class="rounded-full border border-gray-300 bg-gray-100 px-2 py-0.5 text-xs text-gray-600 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-300"> </span></div> <!> <div class="mt-3 flex flex-wrap items-center gap-2 text-xs text-gray-400 dark:text-gray-500"><!> <!> <span> </span></div> <div class="mt-3 flex items-center justify-between"><a target="_blank" rel="noopener noreferrer" class="text-xs text-sky-600 hover:underline dark:text-sky-400"> </a> <!></div></article>'),i0=k('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),l0=k('<div class="flex flex-col gap-3 sm:flex-row"><select class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200"><option>GitHub</option><option>ClawHub</option><option>HuggingFace</option></select> <input type="text" class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 placeholder-gray-400 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:placeholder-gray-500"/> <button type="button" class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"> </button></div> <!>',1),d0=k('<section class="space-y-6"><div class="flex items-center justify-between"><div class="flex items-center gap-3"><h2 class="text-2xl font-semibold"> </h2> <!></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <div class="flex gap-1 rounded-lg border border-gray-200 bg-gray-100/50 p-1 dark:border-gray-700 dark:bg-gray-800/50"><button type="button"> </button> <button type="button"> </button></div> <!> <!> <!></section>');function c0(e,t){$e(t,!0);let r=F("installed"),n=F(kt([])),s=F(!0),o=F(""),l=F(""),d=F("success"),c=F(kt([])),u=F(!1),m=F(""),x=F("github"),w=F(!1),I=F(""),T=F(""),R=F("");function N(j,Q="success"){y(l,j,!0),y(d,Q,!0),setTimeout(()=>{y(l,"")},3e3)}async function O(){try{const j=await wt.getSkills();y(n,Array.isArray(j==null?void 0:j.skills)?j.skills:[],!0),y(o,"")}catch{y(n,[],!0),y(o,"Failed to load skills.")}finally{y(s,!1)}}async function Z(j){try{await wt.toggleSkill(j),y(n,a(n).map(Q=>Q.name===j?{...Q,enabled:!Q.enabled}:Q),!0)}catch{y(n,a(n).map(Q=>Q.name===j?{...Q,enabled:!Q.enabled}:Q),!0)}}async function q(j){if(a(R)!==j){y(R,j,!0);return}y(R,""),y(T,j,!0);try{await wt.uninstallSkill(j),y(n,a(n).filter(Q=>Q.name!==j),!0),N(_("skills.uninstallSuccess"))}catch(Q){N(_("skills.uninstallFailed")+(Q.message?`: ${Q.message}`:""),"error")}finally{y(T,"")}}const E=re(()=>[...a(n)].sort((j,Q)=>j.enabled===Q.enabled?0:j.enabled?-1:1)),h=re(()=>a(n).filter(j=>j.enabled).length);async function M(){!a(m).trim()&&a(x)==="github"&&y(m,"agent skill"),y(u,!0),y(w,!0);try{const j=await wt.discoverSkills(a(x),a(m));y(c,Array.isArray(j==null?void 0:j.results)?j.results:[],!0)}catch{y(c,[],!0)}finally{y(u,!1)}}function C(j){return a(n).some(Q=>Q.name===j)}async function U(j,Q){y(I,j,!0);try{const ye=await wt.installSkill(j,Q);ye!=null&&ye.skill&&y(n,[...a(n),{...ye.skill,enabled:!0}],!0),N(_("skills.installSuccess"))}catch(ye){N(_("skills.installFailed")+(ye.message?`: ${ye.message}`:""),"error")}finally{y(I,"")}}function ie(j){j.key==="Enter"&&M()}Ut(()=>{O()});var fe=d0(),We=i(fe),Me=i(We),W=i(Me),Y=i(W),be=g(W,2);{var ae=j=>{var Q=Bu(),ye=i(Q);P(st=>p(ye,`${a(h)??""}/${a(n).length??""} ${st??""}`),[()=>_("skills.active")]),v(j,Q)};K(be,j=>{!a(s)&&a(n).length>0&&j(ae)})}var ze=g(Me,2),B=i(ze),V=g(We,2),ve=i(V),Se=i(ve),ee=g(ve,2),X=i(ee),ue=g(V,2);{var oe=j=>{var Q=Wu(),ye=i(Q);P(()=>{tt(Q,1,`rounded-lg px-4 py-2 text-sm ${a(d)==="error"?"border border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300":"border border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300"}`),p(ye,a(l))}),v(j,Q)};K(ue,j=>{a(l)&&j(oe)})}var rt=g(ue,2);{var it=j=>{var Q=Re(),ye=xe(Q);{var st=Ie=>{var Ve=Vu(),ct=i(Ve);P(He=>p(ct,He),[()=>_("skills.loading")]),v(Ie,Ve)},Ze=Ie=>{var Ve=qu(),ct=i(Ve);P(()=>p(ct,a(o))),v(Ie,Ve)},ot=Ie=>{var Ve=Ku(),ct=i(Ve);P(He=>p(ct,He),[()=>_("skills.noSkills")]),v(Ie,Ve)},bt=Ie=>{var Ve=Qu();at(Ve,21,()=>a(E),lt,(ct,He)=>{var yt=Xu(),le=i(yt),Le=i(le),he=i(Le),Ne=g(Le,2),D=i(Ne),G=g(le,2);{var qe=z=>{var H=Gu(),de=i(H);P(()=>p(de,a(He).description)),v(z,H)};K(G,z=>{a(He).description&&z(qe)})}var Je=g(G,2),ut=i(Je),mt=g(Je,2),S=i(mt),f=i(S),b=g(S,2);{var A=z=>{var H=Ju(),de=i(H),Pe=i(de),Fe=g(de,2),Ye=i(Fe),Ae=g(Fe,2),ke=i(Ae);P((je,ce,ne)=>{p(Pe,je),Fe.disabled=a(T)===a(He).name,p(Ye,ce),p(ke,ne)},[()=>_("skills.confirmUninstall").replace("{name}",a(He).name),()=>a(T)===a(He).name?_("skills.uninstalling"):_("common.yes"),()=>_("common.no")]),J("click",Fe,()=>q(a(He).name)),J("click",Ae,()=>{y(R,"")}),v(z,H)},L=z=>{var H=Yu(),de=i(H);P(Pe=>p(de,Pe),[()=>_("skills.uninstall")]),J("click",H,()=>q(a(He).name)),v(z,H)};K(b,z=>{a(R)===a(He).name?z(A):z(L,-1)})}P(z=>{p(he,a(He).name),tt(Ne,1,`relative inline-flex h-5 w-9 flex-shrink-0 items-center rounded-full transition ${a(He).enabled?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),tt(D,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(He).enabled?"translate-x-4":"translate-x-1"}`),p(ut,a(He).location),tt(S,1,`rounded-full px-2 py-1 text-xs font-medium ${a(He).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),p(f,z)},[()=>a(He).enabled?_("common.enabled"):_("common.disabled")]),J("click",Ne,()=>Z(a(He).name)),v(ct,yt)}),v(Ie,Ve)};K(ye,Ie=>{a(s)?Ie(st):a(o)?Ie(Ze,1):a(n).length===0?Ie(ot,2):Ie(bt,-1)})}v(j,Q)};K(rt,j=>{a(r)==="installed"&&j(it)})}var Qe=g(rt,2);{var dt=j=>{var Q=l0(),ye=xe(Q),st=i(ye),Ze=i(st);Ze.value=Ze.__value="github";var ot=g(Ze);ot.value=ot.__value="clawhub";var bt=g(ot);bt.value=bt.__value="huggingface";var Ie=g(st,2),Ve=g(Ie,2),ct=i(Ve),He=g(ye,2);{var yt=he=>{var Ne=Zu(),D=i(Ne);P(G=>p(D,G),[()=>_("skills.searching")]),v(he,Ne)},le=he=>{var Ne=e0(),D=i(Ne);P(G=>p(D,G),[()=>_("skills.noResults")]),v(he,Ne)},Le=he=>{var Ne=i0();at(Ne,21,()=>a(c),lt,(D,G)=>{const qe=re(()=>C(a(G).name));var Je=o0(),ut=i(Je),mt=i(ut),S=i(mt),f=i(S),b=g(S,2),A=i(b),L=g(mt,2),z=i(L),H=g(ut,2);{var de=pe=>{var Te=t0(),et=i(Te);P(()=>p(et,a(G).description)),v(pe,Te)};K(H,pe=>{a(G).description&&pe(de)})}var Pe=g(H,2),Fe=i(Pe);{var Ye=pe=>{var Te=r0(),et=g(i(Te));P(()=>p(et,` ${a(G).stars??""}`)),v(pe,Te)};K(Fe,pe=>{a(G).stars>0&&pe(Ye)})}var Ae=g(Fe,2);{var ke=pe=>{var Te=a0(),et=i(Te);P(()=>p(et,a(G).language)),v(pe,Te)};K(Ae,pe=>{a(G).language&&pe(ke)})}var je=g(Ae,2),ce=i(je),ne=g(Pe,2),ge=i(ne),Ue=i(ge),Ee=g(ge,2);{var te=pe=>{var Te=n0(),et=i(Te);P(we=>p(et,we),[()=>_("skills.installed")]),v(pe,Te)},nt=pe=>{var Te=s0(),et=i(Te);P(we=>{Te.disabled=a(I)===a(G).url,p(et,we)},[()=>a(I)===a(G).url?_("skills.installing"):_("skills.install")]),J("click",Te,()=>U(a(G).url,a(G).name)),v(pe,Te)};K(Ee,pe=>{a(qe)?pe(te):pe(nt,-1)})}P((pe,Te,et)=>{p(f,a(G).name),p(A,`${pe??""} ${a(G).owner??""}`),p(z,a(G).source),tt(je,1,a(G).has_license?"text-green-600 dark:text-green-400":"text-yellow-600 dark:text-yellow-400"),p(ce,Te),ht(ge,"href",a(G).url),p(Ue,et)},[()=>_("skills.owner"),()=>a(G).has_license?_("skills.licensed"):_("skills.unlicensed"),()=>a(G).url.replace("https://github.com/","")]),v(D,Je)}),v(he,Ne)};K(He,he=>{a(u)?he(yt):a(w)&&a(c).length===0?he(le,1):a(c).length>0&&he(Le,2)})}P((he,Ne)=>{ht(Ie,"placeholder",he),Ve.disabled=a(u),p(ct,Ne)},[()=>_("skills.search"),()=>a(u)?_("skills.searching"):_("skills.searchBtn")]),In(st,()=>a(x),he=>y(x,he)),J("keydown",Ie,ie),Hr(Ie,()=>a(m),he=>y(m,he)),J("click",Ve,M),v(j,Q)};K(Qe,j=>{a(r)==="discover"&&j(dt)})}P((j,Q,ye,st)=>{p(Y,j),p(B,Q),tt(ve,1,`rounded-md px-4 py-2 text-sm font-medium transition ${a(r)==="installed"?"bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white":"text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"}`),p(Se,ye),tt(ee,1,`rounded-md px-4 py-2 text-sm font-medium transition ${a(r)==="discover"?"bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white":"text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"}`),p(X,st)},[()=>_("skills.title"),()=>_("common.refresh"),()=>_("skills.tabInstalled"),()=>_("skills.tabDiscover")]),J("click",ze,()=>{y(s,!0),O()}),J("click",ve,()=>{y(r,"installed")}),J("click",ee,()=>{y(r,"discover")}),v(e,fe),Ce()}or(["click","keydown"]);var u0=k("<div> </div>"),f0=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),v0=k('<div class="rounded-lg border border-red-200 bg-red-50 p-4 text-sm text-red-600 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400"> </div>'),g0=k('<div class="rounded-lg border border-gray-200 bg-gray-50 p-8 text-center dark:border-gray-700 dark:bg-gray-800"><!> <p class="text-sm text-gray-500 dark:text-gray-400"> </p></div>'),p0=k('<p class="mb-3 text-sm text-gray-600 dark:text-gray-300"> </p>'),b0=k('<span class="rounded-full bg-sky-100 px-2 py-0.5 text-xs text-sky-700 dark:bg-sky-900/30 dark:text-sky-300"> </span>'),y0=k('<div class="mb-3"><p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400"> </p> <div class="flex flex-wrap gap-1"></div></div>'),h0=k('<span class="rounded-full bg-amber-100 px-2 py-0.5 text-xs text-amber-700 dark:bg-amber-900/30 dark:text-amber-300"> </span>'),m0=k('<div class="mb-3"><p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400"> </p> <div class="flex flex-wrap gap-1"></div></div>'),_0=k('<div class="rounded-lg border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="mb-3 flex items-start justify-between"><div><h3 class="font-semibold text-gray-900 dark:text-gray-100"> </h3> <p class="text-xs text-gray-500 dark:text-gray-400"> </p></div> <div><!> <span class="text-xs"> </span></div></div> <!> <!> <!> <div class="flex justify-end"><button type="button" class="flex items-center gap-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-xs text-gray-700 transition hover:bg-gray-100 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200 dark:hover:bg-gray-600"><!> </button></div></div>'),x0=k('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),k0=k('<!> <section class="space-y-6"><div class="flex items-center justify-between"><div class="flex items-center gap-2"><!> <h2 class="text-2xl font-semibold"> </h2></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!></section>',1);function w0(e,t){$e(t,!0);let r=F(kt([])),n=F(!0),s=F(""),o=F(""),l=F(""),d=F("success");function c(W,Y="success"){y(l,W,!0),y(d,Y,!0),setTimeout(()=>{y(l,"")},3e3)}async function u(){y(n,!0);try{const W=await wt.getPlugins();y(r,Array.isArray(W==null?void 0:W.plugins)?W.plugins:[],!0),y(s,"")}catch{y(r,[],!0),y(s,_("plugins.loadFailed"),!0)}finally{y(n,!1)}}async function m(W){y(o,W,!0);try{await wt.reloadPlugin(W),c(_("plugins.reloadSuccess",{name:W})),await u()}catch(Y){c(_("plugins.reloadFailed")+(Y.message?`: ${Y.message}`:""),"error")}finally{y(o,"")}}function x(W){return typeof W=="string"&&W==="Active"?"text-green-500":typeof W=="object"&&(W!=null&&W.Error)?"text-red-500":"text-yellow-500"}function w(W){return typeof W=="string"&&W==="Active"?_("plugins.statusActive"):typeof W=="object"&&(W!=null&&W.Error)?W.Error:_("common.unknown")}Ut(()=>{u()});var I=k0(),T=xe(I);{var R=W=>{var Y=u0(),be=i(Y);P(()=>{tt(Y,1,`fixed right-4 top-4 z-50 rounded-lg px-4 py-2 text-sm font-medium text-white shadow-lg transition ${a(d)==="error"?"bg-red-600":"bg-green-600"}`),p(be,a(l))}),v(W,Y)};K(T,W=>{a(l)&&W(R)})}var N=g(T,2),O=i(N),Z=i(O),q=i(Z);bs(q,{size:24});var E=g(q,2),h=i(E),M=g(Z,2),C=i(M),U=g(O,2);{var ie=W=>{var Y=f0(),be=i(Y);P(ae=>p(be,ae),[()=>_("plugins.loading")]),v(W,Y)},fe=W=>{var Y=v0(),be=i(Y);P(()=>p(be,a(s))),v(W,Y)},We=W=>{var Y=g0(),be=i(Y);bs(be,{size:40,class:"mx-auto mb-3 text-gray-400 dark:text-gray-500"});var ae=g(be,2),ze=i(ae);P(B=>p(ze,B),[()=>_("plugins.noPlugins")]),v(W,Y)},Me=W=>{var Y=x0();at(Y,21,()=>a(r),lt,(be,ae)=>{var ze=_0(),B=i(ze),V=i(B),ve=i(V),Se=i(ve),ee=g(ve,2),X=i(ee),ue=g(V,2),oe=i(ue);{var rt=le=>{gd(le,{size:16})},it=le=>{vd(le,{size:16})};K(oe,le=>{typeof a(ae).status=="string"&&a(ae).status==="Active"?le(rt):le(it,-1)})}var Qe=g(oe,2),dt=i(Qe),j=g(B,2);{var Q=le=>{var Le=p0(),he=i(Le);P(()=>p(he,a(ae).description)),v(le,Le)};K(j,le=>{a(ae).description&&le(Q)})}var ye=g(j,2);{var st=le=>{var Le=y0(),he=i(Le),Ne=i(he),D=g(he,2);at(D,21,()=>a(ae).capabilities,lt,(G,qe)=>{var Je=b0(),ut=i(Je);P(()=>p(ut,a(qe))),v(G,Je)}),P(G=>p(Ne,G),[()=>_("plugins.capabilities")]),v(le,Le)};K(ye,le=>{var Le;(Le=a(ae).capabilities)!=null&&Le.length&&le(st)})}var Ze=g(ye,2);{var ot=le=>{var Le=m0(),he=i(Le),Ne=i(he),D=g(he,2);at(D,21,()=>a(ae).permissions_required,lt,(G,qe)=>{var Je=h0(),ut=i(Je);P(()=>p(ut,a(qe))),v(G,Je)}),P(G=>p(Ne,G),[()=>_("plugins.permissions")]),v(le,Le)};K(Ze,le=>{var Le;(Le=a(ae).permissions_required)!=null&&Le.length&&le(ot)})}var bt=g(Ze,2),Ie=i(bt),Ve=i(Ie);{var ct=le=>{kd(le,{size:14,class:"animate-spin"})},He=le=>{Ao(le,{size:14})};K(Ve,le=>{a(o)===a(ae).name?le(ct):le(He,-1)})}var yt=g(Ve);P((le,Le,he)=>{p(Se,a(ae).name),p(X,`v${a(ae).version??""}`),tt(ue,1,`flex items-center gap-1 ${le??""}`),p(dt,Le),Ie.disabled=a(o)===a(ae).name,p(yt,` ${he??""}`)},[()=>x(a(ae).status),()=>w(a(ae).status),()=>_("plugins.reload")]),J("click",Ie,()=>m(a(ae).name)),v(be,ze)}),v(W,Y)};K(U,W=>{a(n)?W(ie):a(s)?W(fe,1):a(r).length===0?W(We,2):W(Me,-1)})}P((W,Y)=>{p(h,W),p(C,Y)},[()=>_("plugins.title"),()=>_("common.refresh")]),J("click",M,u),v(e,I),Ce()}or(["click"]);var S0=k('<button type="button" class="fixed inset-0 z-30 bg-black/30 dark:bg-black/60 lg:hidden"></button>'),A0=k('<button type="button"> </button>'),E0=k('<section class="space-y-4"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></section>'),$0=k('<div class="flex min-h-screen"><!> <aside><div class="mb-4 border-b border-gray-200 pb-4 dark:border-gray-700"><p class="text-lg font-semibold"> </p></div> <nav class="space-y-1"></nav></aside> <div class="flex min-w-0 flex-1 flex-col"><header class="sticky top-0 z-20 flex items-center justify-between border-b border-gray-200 bg-white/95 px-4 py-3 backdrop-blur dark:border-gray-700 dark:bg-gray-900/95"><div class="flex items-center gap-3"><button type="button" class="rounded-lg border border-gray-300 px-2 py-1 text-sm text-gray-700 dark:border-gray-700 dark:text-gray-200 lg:hidden"> </button> <h1 class="text-lg font-semibold"> </h1></div> <div class="flex items-center gap-2"><button type="button" aria-label="Toggle theme" class="rounded-lg border border-gray-300 bg-white p-2 text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"><!></button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></header> <main class="flex-1 p-4 sm:p-6"><!></main></div></div>'),C0=k('<div class="min-h-screen bg-gray-50 text-gray-900 dark:bg-gray-900 dark:text-gray-100"><!></div>');function M0(e,t){$e(t,!0);let r=F(kt(So())),n=F(kt(Fa())),s=F(!1),o=F(!0);const l=re(()=>a(n).length>0),d=re(()=>a(l)&&a(r)==="/"?"/overview":a(r)),c=re(()=>a(d).startsWith("/chat/")?"/sessions":a(d));function u(C){try{return decodeURIComponent(C)}catch{return C}}const m=re(()=>a(d).startsWith("/chat/")?u(a(d).slice(6)):"");function x(){localStorage.getItem("prx-console-theme")==="light"?y(o,!1):y(o,!0),w()}function w(){a(o)?document.documentElement.classList.add("dark"):document.documentElement.classList.remove("dark")}function I(){y(o,!a(o)),localStorage.setItem("prx-console-theme",a(o)?"dark":"light"),w()}function T(){y(n,Fa(),!0)}function R(C){y(r,C,!0),y(s,!1)}function N(C){y(n,C,!0),Sr("/overview",!0)}function O(){xo(),y(n,""),Sr("/",!0)}function Z(C){Sr(C)}Ut(()=>{x();const C=nd(R),U=ie=>{if(ie.key==="prx-console-token"){T();return}if(ie.key===ln&&ad(),ie.key==="prx-console-theme"){const fe=localStorage.getItem("prx-console-theme");y(o,fe!=="light"),w()}};return window.addEventListener("storage",U),()=>{C(),window.removeEventListener("storage",U)}}),Ut(()=>{if(a(l)&&a(r)==="/"){Sr("/overview",!0);return}!a(l)&&a(r)!=="/"&&Sr("/",!0)});var q=C0(),E=i(q);{var h=C=>{Od(C,{onLogin:N})},M=C=>{var U=$0(),ie=i(U);{var fe=D=>{var G=S0();P(qe=>ht(G,"aria-label",qe),[()=>_("app.closeSidebar")]),J("click",G,()=>y(s,!1)),v(D,G)};K(ie,D=>{a(s)&&D(fe)})}var We=g(ie,2),Me=i(We),W=i(Me),Y=i(W),be=g(Me,2);at(be,21,()=>wl,lt,(D,G)=>{var qe=A0(),Je=i(qe);P(ut=>{tt(qe,1,`w-full rounded-lg px-3 py-2 text-left text-sm transition ${a(c)===a(G).path?"bg-sky-600 text-white":"text-gray-600 hover:bg-gray-100 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-gray-100"}`),p(Je,ut)},[()=>_(a(G).labelKey)]),J("click",qe,()=>Z(a(G).path)),v(D,qe)});var ae=g(We,2),ze=i(ae),B=i(ze),V=i(B),ve=i(V),Se=g(V,2),ee=i(Se),X=g(B,2),ue=i(X),oe=i(ue);{var rt=D=>{Md(D,{size:16})},it=D=>{Sd(D,{size:16})};K(oe,D=>{a(o)?D(rt):D(it,-1)})}var Qe=g(ue,2),dt=i(Qe),j=g(Qe,2),Q=i(j),ye=g(ze,2),st=i(ye);{var Ze=D=>{qd(D,{})},ot=D=>{ec(D,{})},bt=D=>{bc(D,{get sessionId(){return a(m)}})},Ie=re(()=>a(d).startsWith("/chat/")),Ve=D=>{Sc(D,{})},ct=D=>{Pu(D,{})},He=D=>{Uu(D,{})},yt=D=>{c0(D,{})},le=D=>{w0(D,{})},Le=D=>{gu(D,{})},he=D=>{mu(D,{})},Ne=D=>{var G=E0(),qe=i(G),Je=i(qe),ut=g(qe,2),mt=i(ut);P((S,f)=>{p(Je,S),p(mt,f)},[()=>_("app.notFound"),()=>_("app.backToOverview")]),J("click",ut,()=>Z("/overview")),v(D,G)};K(st,D=>{a(d)==="/overview"?D(Ze):a(d)==="/sessions"?D(ot,1):a(Ie)?D(bt,2):a(d)==="/channels"?D(Ve,3):a(d)==="/hooks"?D(ct,4):a(d)==="/mcp"?D(He,5):a(d)==="/skills"?D(yt,6):a(d)==="/plugins"?D(le,7):a(d)==="/config"?D(Le,8):a(d)==="/logs"?D(he,9):D(Ne,-1)})}P((D,G,qe,Je,ut)=>{tt(We,1,`fixed inset-y-0 left-0 z-40 w-64 border-r border-gray-200 bg-white p-4 transition-transform dark:border-gray-700 dark:bg-gray-800 lg:static lg:translate-x-0 ${a(s)?"translate-x-0":"-translate-x-full"}`),p(Y,D),p(ve,G),p(ee,qe),ht(Qe,"aria-label",Je),p(dt,Yr.lang==="zh"?"中文 / EN":"EN / 中文"),p(Q,ut)},[()=>_("app.title"),()=>_("app.menu"),()=>_("app.title"),()=>_("app.language"),()=>_("common.logout")]),J("click",V,()=>y(s,!a(s))),J("click",ue,I),J("click",Qe,function(...D){ta==null||ta.apply(this,D)}),J("click",j,O),v(C,U)};K(E,C=>{a(l)?C(M,-1):C(h)})}v(e,q),Ce()}or(["click"]);Qi(M0,{target:document.getElementById("app")});
