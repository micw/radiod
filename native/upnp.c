/* SPDX-License-Identifier: MIT -- original radiod implementation. */
#include "upnp.h"
#include <libgupnp/gupnp.h>
#include <libsoup/soup.h>
#include <string.h>

#define SINK "http-get:*:audio/mpeg:*,http-get:*:audio/aac:*,http-get:*:audio/mp4:*,http-get:*:audio/x-wav:*,http-get:*:audio/wav:*,http-get:*:audio/ogg:*,http-get:*:application/ogg:*"
typedef struct {
    const radiod_upnp_callbacks *cb;
    void *user;
    GMainLoop *loop;
    GUPnPService *av, *rc, *cm;
    radiod_upnp_snapshot last;
    char *av_event, *rc_event;
} Server;
static const char *state(int n) {
    static const char *s[] = {"STOPPED", "TRANSITIONING", "PLAYING", "PAUSED_PLAYBACK"};
    return n >= 0 && n < 4 ? s[n] : "STOPPED";
}
static void snapshot(Server *s, radiod_upnp_snapshot *v) {
    memset(v, 0, sizeof(*v));
    s->cb->snapshot(s->user, v);
    v->uri[sizeof(v->uri)-1] = 0;
    v->metadata[sizeof(v->metadata)-1] = 0;
    v->volume = CLAMP(v->volume, 0, 100);
    v->mute = !!v->mute;
}
static char *clock_time(uint32_t t) {
    return g_strdup_printf("%u:%02u:%02u", t/3600, (t/60)%60, t%60);
}
static const char *actions(const radiod_upnp_snapshot *v) {
    if (!v->uri[0]) return "";
    if (v->transport_state == 2) return "Pause,Stop";
    if (v->transport_state == 1) return "Stop";
    return "Play,Stop";
}
static void field(GString *x, const char *name, const char *value, gboolean channel) {
    char *e = g_markup_escape_text(value, -1);
    g_string_append_printf(x, "<%s%s val=\"%s\"/>", name, channel ? " channel=\"Master\"" : "", e);
    g_free(e);
}
static char *event_xml(const radiod_upnp_snapshot *v, gboolean rc) {
    GString *x = g_string_new(rc ? "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/RCS/\"><InstanceID val=\"0\">" : "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\"><InstanceID val=\"0\">");
    char *duration = clock_time(v->duration_seconds), *position = clock_time(v->position_seconds);
    char number[32];
    if (rc) {
        g_snprintf(number, sizeof(number), "%d", v->volume);
        field(x,"Volume",number,TRUE); field(x,"Mute",v->mute ? "1" : "0",TRUE);
    } else {
        field(x,"TransportState",state(v->transport_state),FALSE);
        field(x,"TransportStatus",v->transport_error ? "ERROR_OCCURRED" : "OK",FALSE);
        field(x,"TransportPlaySpeed","1",FALSE);
        field(x,"AVTransportURI",v->uri,FALSE);
        field(x,"AVTransportURIMetaData",v->metadata,FALSE);
        field(x,"CurrentTrackURI",v->uri,FALSE);
        field(x,"CurrentTrackMetaData",v->metadata,FALSE);
        field(x,"CurrentTrackDuration",duration,FALSE);
        field(x,"CurrentMediaDuration",duration,FALSE);
        field(x,"RelativeTimePosition",position,FALSE);
        field(x,"AbsoluteTimePosition",position,FALSE);
        field(x,"CurrentTrack",v->uri[0] ? "1" : "0",FALSE);
        field(x,"NumberOfTracks",v->uri[0] ? "1" : "0",FALSE);
        field(x,"CurrentTransportActions",actions(v),FALSE);
        field(x,"CurrentPlayMode","NORMAL",FALSE);
        field(x,"PlaybackStorageMedium","NETWORK",FALSE);
        field(x,"RecordStorageMedium","NOT_IMPLEMENTED",FALSE);
    }
    g_free(duration); g_free(position);
    g_string_append(x,"</InstanceID></Event>");
    return g_string_free(x,FALSE);
}
static void refresh(Server *s, gboolean notify) {
    radiod_upnp_snapshot v;
    snapshot(s,&v);
    char *av = event_xml(&v,FALSE), *rc = event_xml(&v,TRUE);
    if (notify && g_strcmp0(av,s->av_event)) gupnp_service_notify(s->av,"LastChange",G_TYPE_STRING,av,NULL);
    if (notify && g_strcmp0(rc,s->rc_event)) gupnp_service_notify(s->rc,"LastChange",G_TYPE_STRING,rc,NULL);
    g_free(s->av_event); g_free(s->rc_event);
    s->av_event=av; s->rc_event=rc; s->last=v;
}
static gboolean tick(gpointer data) {
    Server *s=data;
    if (s->cb->should_stop && s->cb->should_stop(s->user)) { g_main_loop_quit(s->loop); return G_SOURCE_REMOVE; }
    refresh(s,TRUE); return G_SOURCE_CONTINUE;
}
static void query(GUPnPService *service, const char *name, GValue *value, gpointer data) {
    Server *s=data;
    refresh(s,TRUE);
    g_value_init(value,G_TYPE_STRING);
    const char *v="";
    if (!strcmp(name,"LastChange")) v=service==s->rc ? s->rc_event : s->av_event;
    else if (!strcmp(name,"SinkProtocolInfo")) v=SINK;
    else if (!strcmp(name,"CurrentConnectionIDs")) v="0";
    g_value_set_string(value,v);
}
static void out_s(GUPnPServiceAction *a,const char *n,const char *v) { gupnp_service_action_set(a,n,G_TYPE_STRING,v,NULL); }
static void out_u(GUPnPServiceAction *a,const char *n,guint v) { gupnp_service_action_set(a,n,G_TYPE_UINT,v,NULL); }
static void out_i(GUPnPServiceAction *a,const char *n,gint v) { gupnp_service_action_set(a,n,G_TYPE_INT,v,NULL); }
static void fail(GUPnPServiceAction *a,int code,const char *text) { gupnp_service_action_return_error(a,(guint)code,text); }
/* Read as strings first: numeric getters otherwise default missing values to
 * zero. A missing argument must never accidentally change volume or mute. */
static char *input(GUPnPServiceAction *a,const char *name) {
    GValue value=G_VALUE_INIT;
    g_value_init(&value,G_TYPE_STRING);
    gupnp_service_action_get_value(a,name,&value);
    char *text=g_value_dup_string(&value);
    g_value_unset(&value);
    return text;
}
static gboolean input_uint(GUPnPServiceAction *a,const char *name,guint64 maximum,guint *number) {
    char *text=input(a,name);
    guint64 value=0;
    gboolean ok=text && g_ascii_string_to_unsigned(text,10,0,maximum,&value,NULL);
    g_free(text);
    if (ok) *number=(guint)value;
    return ok;
}
static void action(GUPnPService *service,GUPnPServiceAction *a,gpointer data) {
    Server *s=data;
    const char *n=gupnp_service_action_get_name(a);
    guint id=0;
    if (service!=s->cm) {
        if (!input_uint(a,"InstanceID",G_MAXUINT,&id)) { fail(a,402,"Missing or invalid InstanceID"); return; }
        if (id) { fail(a,718,"Invalid InstanceID"); return; }
    }
    if (service==s->rc && (strstr(n,"Volume") || strstr(n,"Mute"))) {
        char *channel=NULL;
        gupnp_service_action_get(a,"Channel",G_TYPE_STRING,&channel,NULL);
        gboolean ok=g_strcmp0(channel,"Master")==0; g_free(channel);
        if (!ok) { fail(a,402,"Invalid channel"); return; }
    }
    refresh(s,TRUE);
    radiod_upnp_snapshot *v=&s->last;
    gboolean mutate=!strcmp(n,"SetAVTransportURI") || !strcmp(n,"Play") || !strcmp(n,"Pause") || !strcmp(n,"Stop") || !strcmp(n,"SetVolume") || !strcmp(n,"SetMute");
    if (mutate) {
        char *uri=NULL,*metadata=NULL,*speed=NULL;
        int value=0;
        if (!strcmp(n,"SetAVTransportURI")) {
            gupnp_service_action_get(a,"CurrentURI",G_TYPE_STRING,&uri,"CurrentURIMetaData",G_TYPE_STRING,&metadata,NULL);
            if (!uri || !metadata || strlen(uri)>=RADIOD_UPNP_URI_CAP || strlen(metadata)>=RADIOD_UPNP_METADATA_CAP || !g_utf8_validate(uri,-1,NULL) || !g_utf8_validate(metadata,-1,NULL)) {
                g_free(uri); g_free(metadata); fail(a,402,"Invalid or oversized URI/metadata"); return;
            }
        }
        if (!strcmp(n,"Play")) {
            speed=input(a,"Speed");
            if (!speed) { fail(a,402,"Missing Speed"); return; }
            gboolean ok=g_strcmp0(speed,"1")==0; g_free(speed);
            if (!ok) { fail(a,717,"Play speed not supported"); return; }
        }
        if (!strcmp(n,"SetVolume")) {
            guint volume=0;
            if (!input_uint(a,"DesiredVolume",100,&volume)) { fail(a,402,"Missing or invalid volume"); return; }
            value=(int)volume;
        }
        if (!strcmp(n,"SetMute")) {
            char *mute=input(a,"DesiredMute");
            gboolean on=mute && (!strcmp(mute,"1") || !g_ascii_strcasecmp(mute,"true") || !g_ascii_strcasecmp(mute,"yes"));
            gboolean off=mute && (!strcmp(mute,"0") || !g_ascii_strcasecmp(mute,"false") || !g_ascii_strcasecmp(mute,"no"));
            g_free(mute);
            if (!on && !off) { fail(a,402,"Missing or invalid mute"); return; }
            value=on;
        }
        SoupServerMessage *message=gupnp_service_action_get_message(a);
        GSocketAddress *remote=soup_server_message_get_remote_address(message);
        char *ip=G_IS_INET_SOCKET_ADDRESS(remote) ? g_inet_address_to_string(g_inet_socket_address_get_address(G_INET_SOCKET_ADDRESS(remote))) : g_strdup("");
        g_object_unref(message);
        char error[256]={0};
        int code=s->cb->action(s->user,n,ip,uri ? uri : "",metadata ? metadata : "",value,error,sizeof(error));
        error[sizeof(error)-1]=0;
        g_free(ip); g_free(uri); g_free(metadata);
        if (code) { fail(a,code>0 ? code : 501,error[0] && g_utf8_validate(error,-1,NULL) ? error : "Action failed"); return; }
        refresh(s,TRUE);
    } else if (!strcmp(n,"GetTransportInfo")) {
        out_s(a,"CurrentTransportState",state(v->transport_state)); out_s(a,"CurrentTransportStatus",v->transport_error ? "ERROR_OCCURRED" : "OK"); out_s(a,"CurrentSpeed","1");
    } else if (!strcmp(n,"GetMediaInfo")) {
        char *t=clock_time(v->duration_seconds);
        out_u(a,"NrTracks",v->uri[0] ? 1 : 0); out_s(a,"MediaDuration",t); g_free(t);
        out_s(a,"CurrentURI",v->uri); out_s(a,"CurrentURIMetaData",v->metadata); out_s(a,"NextURI",""); out_s(a,"NextURIMetaData","");
        out_s(a,"PlayMedium","NETWORK"); out_s(a,"RecordMedium","NOT_IMPLEMENTED"); out_s(a,"WriteStatus","NOT_IMPLEMENTED");
    } else if (!strcmp(n,"GetPositionInfo")) {
        char *t=clock_time(v->duration_seconds), *p=clock_time(v->position_seconds);
        out_u(a,"Track",v->uri[0] ? 1 : 0); out_s(a,"TrackDuration",t); out_s(a,"TrackMetaData",v->metadata); out_s(a,"TrackURI",v->uri);
        out_s(a,"RelTime",p); out_s(a,"AbsTime",p); out_i(a,"RelCount",-1); out_i(a,"AbsCount",-1); g_free(t); g_free(p);
    } else if (!strcmp(n,"GetDeviceCapabilities")) {
        out_s(a,"PlayMedia","NETWORK"); out_s(a,"RecMedia","NOT_IMPLEMENTED"); out_s(a,"RecQualityModes","NOT_IMPLEMENTED");
    } else if (!strcmp(n,"GetTransportSettings")) {
        out_s(a,"PlayMode","NORMAL"); out_s(a,"RecQualityMode","NOT_IMPLEMENTED");
    } else if (!strcmp(n,"GetCurrentTransportActions")) out_s(a,"Actions",actions(v));
    else if (!strcmp(n,"GetVolume")) out_u(a,"CurrentVolume",(guint)v->volume);
    else if (!strcmp(n,"GetMute")) gupnp_service_action_set(a,"CurrentMute",G_TYPE_BOOLEAN,v->mute,NULL);
    else if (!strcmp(n,"ListPresets")) out_s(a,"CurrentPresetNameList","FactoryDefaults");
    else if (!strcmp(n,"SelectPreset")) {
        char *preset=input(a,"PresetName");
        gboolean present=preset && preset[0]; g_free(preset);
        fail(a,present ? 701 : 402,present ? "Preset selection not supported" : "Missing PresetName"); return;
    }
    else if (!strcmp(n,"GetProtocolInfo")) { out_s(a,"Source",""); out_s(a,"Sink",SINK); }
    else if (!strcmp(n,"GetCurrentConnectionIDs")) out_s(a,"ConnectionIDs","0");
    else if (!strcmp(n,"GetCurrentConnectionInfo")) {
        char *connection=input(a,"ConnectionID"); gint64 number=0;
        gboolean valid=connection && g_ascii_string_to_signed(connection,10,G_MININT32,G_MAXINT32,&number,NULL);
        g_free(connection);
        if (!valid) { fail(a,402,"Missing or invalid ConnectionID"); return; }
        if (number!=0) { fail(a,706,"Invalid connection reference"); return; }
        out_i(a,"RcsID",0); out_i(a,"AVTransportID",0); out_s(a,"ProtocolInfo",""); out_s(a,"PeerConnectionManager","");
        out_i(a,"PeerConnectionID",-1); out_s(a,"Direction","Input"); out_s(a,"Status","OK");
    } else { fail(a,!strcmp(n,"Seek") ? 710 : 401,"Action not supported"); return; }
    gupnp_service_action_return_success(a);
}
#define HTTP_BODY_LIMIT 32768
static void reject_body(SoupServerMessage *message) {
    SoupMessageBody *body=soup_server_message_get_request_body(message);
    soup_message_body_set_accumulate(body,FALSE);
    soup_message_body_truncate(body);
    soup_server_message_set_status(message,SOUP_STATUS_REQUEST_ENTITY_TOO_LARGE,NULL);
    soup_message_headers_replace(soup_server_message_get_response_headers(message),"Connection","close");
}
/* Preflight callbacks before GUPnP: only numeric IPv4 HTTP destinations in
 * the bound interface's network segment. No DNS, userinfo or fragments. This
 * also prevents GUPnP's invalid-callback warning from logging raw URLs. This
 * is not Rust's CIDR authorization and does not constrain HTTP redirects. */
static gboolean callbacks_valid(GUPnPContext *context,const char *callbacks) {
    if (strlen(callbacks)>2048) return FALSE;
    const char *p=callbacks;
    guint count=0;
    while (*p) {
        while (g_ascii_isspace(*p)) p++;
        if (!*p) break;
        if (*p!='<' || ++count>6) return FALSE;
        const char *end=strchr(++p,'>');
        if (!end || end==p || end-p>256) return FALSE;
        char *text=g_strndup(p,(gsize)(end-p));
        GUri *uri=g_uri_parse(text,G_URI_FLAGS_NONE,NULL);
        g_free(text);
        gboolean ok=uri && !g_strcmp0(g_uri_get_scheme(uri),"http") && !g_uri_get_userinfo(uri) && !g_uri_get_fragment(uri);
        const char *host=uri ? g_uri_get_host(uri) : NULL;
        GInetAddress *ip=host ? g_inet_address_new_from_string(host) : NULL;
        ok=ok && ip && g_inet_address_get_family(ip)==G_SOCKET_FAMILY_IPV4 && !g_inet_address_get_is_any(ip) && !g_inet_address_get_is_multicast(ip);
        if (ok) {
            GSocketAddress *address=g_inet_socket_address_new(ip,0);
            ok=gssdp_client_can_reach(GSSDP_CLIENT(context),G_INET_SOCKET_ADDRESS(address));
            g_object_unref(address);
        }
        g_clear_object(&ip);
        if (uri) g_uri_unref(uri);
        if (!ok) return FALSE;
        p=end+1;
    }
    return count>0;
}
static void received_headers(SoupServerMessage *message,gpointer data) {
    SoupMessageHeaders *headers=soup_server_message_get_request_headers(message);
    if (soup_message_headers_get_content_length(headers)>HTTP_BODY_LIMIT) { reject_body(message); return; }
    if (!strcmp(soup_server_message_get_method(message),"SUBSCRIBE")) {
        const char *callbacks=soup_message_headers_get_one(headers,"Callback");
        if (callbacks && !callbacks_valid(GUPNP_CONTEXT(data),callbacks))
            soup_server_message_set_status(message,SOUP_STATUS_PRECONDITION_FAILED,NULL);
    }
}
static void received_chunk(SoupServerMessage *message,GBytes *chunk,gpointer data) {
    guint64 *total=data;
    *total+=g_bytes_get_size(chunk);
    if (*total>HTTP_BODY_LIMIT) reject_body(message);
}
static void request_started(SoupServer *server,SoupServerMessage *message,gpointer data) {
    (void)server;
    guint64 *total=g_new0(guint64,1);
    g_object_set_data_full(G_OBJECT(message),"radiod-body-size",total,g_free);
    g_signal_connect(message,"got-headers",G_CALLBACK(received_headers),data);
    g_signal_connect(message,"got-chunk",G_CALLBACK(received_chunk),total);
}
static void description(SoupServer *server,SoupServerMessage *message,const char *path,GHashTable *query,gpointer data) {
    (void)server; (void)path; (void)query;
    const char *method=soup_server_message_get_method(message);
    if (strcmp(method,"GET") && strcmp(method,"HEAD")) {
        soup_server_message_set_status(message,SOUP_STATUS_METHOD_NOT_ALLOWED,NULL); return;
    }
    const char *xml=data;
    soup_server_message_set_response(message,"text/xml; charset=\"utf-8\"",SOUP_MEMORY_COPY,xml,strlen(xml));
    soup_server_message_set_status(message,SOUP_STATUS_OK,NULL);
}
static char *replace(const char *text,const char *key,const char *value) {
    char **parts=g_strsplit(text,key,-1); char *r=g_strjoinv(value,parts); g_strfreev(parts); return r;
}
int radiod_upnp_run(const radiod_upnp_config *config,const radiod_upnp_callbacks *cb,void *user,char *error_buffer,size_t error_length) {
    Server s={.cb=cb,.user=user};
    GError *error=NULL;
    GMainContext *main=g_main_context_new();
    GUPnPContext *context=NULL; GUPnPRootDevice *root=NULL; GUPnPXMLDoc *doc=NULL;
    GInetAddress *address=NULL; GSource *timer=NULL;
    char *path=NULL,*xml=NULL,*name=NULL,*udn=NULL,*escaped=NULL,*rendered=NULL;
    int result=-1;
    g_main_context_push_thread_default(main);
    if (!config || !cb || !cb->ready || !cb->action || !cb->snapshot || !config->bind_address || !config->asset_dir || !config->friendly_name || !config->uuid || !config->http_port) {
        g_set_error_literal(&error,G_IO_ERROR,G_IO_ERROR_INVALID_ARGUMENT,"Missing UPnP configuration or callbacks"); goto done;
    }
    const char *uuid=g_str_has_prefix(config->uuid,"uuid:") ? config->uuid+5 : config->uuid;
    if (!g_uuid_string_is_valid(uuid) || !g_utf8_validate(config->friendly_name,-1,NULL) || strlen(config->friendly_name)>256) {
        g_set_error_literal(&error,G_IO_ERROR,G_IO_ERROR_INVALID_ARGUMENT,"Invalid UUID or friendly name"); goto done;
    }
    address=g_inet_address_new_from_string(config->bind_address);
    if (!address || g_inet_address_get_family(address)!=G_SOCKET_FAMILY_IPV4 || g_inet_address_get_is_any(address) || g_inet_address_get_is_multicast(address)) {
        g_set_error_literal(&error,G_IO_ERROR,G_IO_ERROR_INVALID_ARGUMENT,"A specific IPv4 bind address is required"); goto done;
    }
    context=gupnp_context_new_full(config->interface_name && config->interface_name[0] ? config->interface_name : NULL,address,config->http_port,GSSDP_UDA_VERSION_1_0,&error);
    if (!context) goto done;
    g_signal_connect(gupnp_context_get_server(context),"request-started",G_CALLBACK(request_started),context);
    path=g_build_filename(config->asset_dir,"device.xml",NULL);
    gsize length=0;
    if (!g_file_get_contents(path,&xml,&length,&error)) goto done;
    if (length>65536) { g_set_error_literal(&error,G_IO_ERROR,G_IO_ERROR_INVALID_DATA,"Device description too large"); goto done; }
    name=g_markup_escape_text(config->friendly_name,-1);
    udn=g_strconcat("uuid:",uuid,NULL); escaped=g_markup_escape_text(udn,-1);
    rendered=replace(xml,"@NAME@",name); g_free(xml); xml=replace(rendered,"@UDN@",escaped);
    xmlDoc *parsed=xmlReadMemory(xml,(int)strlen(xml),"device.xml",NULL,XML_PARSE_NONET);
    if (!parsed) { g_set_error_literal(&error,G_IO_ERROR,G_IO_ERROR_INVALID_DATA,"Invalid device XML"); goto done; }
    doc=gupnp_xml_doc_new(parsed);
    root=gupnp_root_device_new_full(context,gupnp_resource_factory_get_default(),doc,"device.xml",config->asset_dir,&error);
    if (!root) goto done;
    /* GUPnP hosts description_path from disk even when description_doc was
     * supplied. Override both routes so controllers never see the template. */
    char *description_path=g_strconcat("/",gupnp_root_device_get_description_document_name(root),NULL);
    gupnp_context_unhost_path(context,description_path);
    soup_server_add_handler(gupnp_context_get_server(context),description_path,description,g_strdup(xml),g_free);
    soup_server_add_handler(gupnp_context_get_server(context),"/device.xml",description,g_strdup(xml),g_free);
    g_free(description_path);
    s.av=GUPNP_SERVICE(gupnp_device_info_get_service(GUPNP_DEVICE_INFO(root),"urn:schemas-upnp-org:service:AVTransport:1"));
    s.rc=GUPNP_SERVICE(gupnp_device_info_get_service(GUPNP_DEVICE_INFO(root),"urn:schemas-upnp-org:service:RenderingControl:1"));
    s.cm=GUPNP_SERVICE(gupnp_device_info_get_service(GUPNP_DEVICE_INFO(root),"urn:schemas-upnp-org:service:ConnectionManager:1"));
    if (!s.av || !s.rc || !s.cm) { g_set_error_literal(&error,G_IO_ERROR,G_IO_ERROR_INVALID_DATA,"Missing renderer services"); goto done; }
    GUPnPService *services[]={s.av,s.rc,s.cm};
    static const char *const av_actions[]={"SetAVTransportURI","Play","Pause","Stop","GetTransportInfo","GetMediaInfo","GetPositionInfo","GetDeviceCapabilities","GetTransportSettings","GetCurrentTransportActions",NULL};
    static const char *const rc_actions[]={"GetVolume","SetVolume","GetMute","SetMute","ListPresets","SelectPreset",NULL};
    static const char *const cm_actions[]={"GetProtocolInfo","GetCurrentConnectionIDs","GetCurrentConnectionInfo",NULL};
    const char *const *action_names[]={av_actions,rc_actions,cm_actions};
    static const char *const av_variables[]={"LastChange",NULL};
    static const char *const rc_variables[]={"LastChange",NULL};
    static const char *const cm_variables[]={"SourceProtocolInfo","SinkProtocolInfo","CurrentConnectionIDs",NULL};
    const char *const *variable_names[]={av_variables,rc_variables,cm_variables};
    /* GUPnP gates dispatch using g_signal_has_handler_pending with the action
     * detail quark. Register explicit details rather than generic handlers. */
    for (guint i=0;i<G_N_ELEMENTS(services);i++) {
        for (guint j=0;action_names[i][j];j++) {
            char *signal=g_strconcat("action-invoked::",action_names[i][j],NULL);
            g_signal_connect(services[i],signal,G_CALLBACK(action),&s);
            g_free(signal);
        }
        for (guint j=0;variable_names[i][j];j++) {
            char *signal=g_strconcat("query-variable::",variable_names[i][j],NULL);
            g_signal_connect(services[i],signal,G_CALLBACK(query),&s);
            g_free(signal);
        }
    }
    s.loop=g_main_loop_new(main,FALSE); refresh(&s,FALSE);
    timer=g_timeout_source_new(100); g_source_set_callback(timer,tick,&s,NULL); g_source_attach(timer,main);
    gupnp_root_device_set_available(root,TRUE);
    cb->ready(user,1,"");
    g_main_loop_run(s.loop); result=0;
done:
    if (result) {
        /* Deliberately do not expose/log library errors that could contain URLs. */
        const char *message="UPnP startup failed (check interface, IPv4 address, HTTP port and XML assets)";
        if (error_buffer && error_length) g_strlcpy(error_buffer,message,error_length);
        if (cb && cb->ready) cb->ready(user,0,message);
    } else if (error_buffer && error_length) error_buffer[0]=0;
    if (timer) { g_source_destroy(timer); g_source_unref(timer); }
    if (root) gupnp_root_device_set_available(root,FALSE);
    g_clear_object(&s.av); g_clear_object(&s.rc); g_clear_object(&s.cm);
    g_clear_object(&root); g_clear_object(&doc); g_clear_object(&context); g_clear_object(&address);
    if (s.loop) g_main_loop_unref(s.loop);
    g_free(s.av_event); g_free(s.rc_event); g_clear_error(&error);
    g_free(path); g_free(xml); g_free(name); g_free(udn); g_free(escaped); g_free(rendered);
    g_main_context_pop_thread_default(main); g_main_context_unref(main);
    return result;
}
