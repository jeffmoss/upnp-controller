use anyhow::Result;
use kube::{
    api::{Api, Patch, PatchParams},
    Client,
};
use k8s_openapi::api::core::v1::Node;
use serde_json::json;

#[allow(dead_code)]
const ANNOTATION_KEY: &str = "upnp.k8s.io/wan-ip";

#[allow(dead_code)]
pub async fn annotate_node_with_wan_ip(client: &Client, node_name: &str, wan_ip: &str) -> Result<()> {
    let api: Api<Node> = Api::all(client.clone());
    let patch = json!({
        "metadata": {
            "annotations": {
                ANNOTATION_KEY: wan_ip
            }
        }
    });
    api.patch(
        node_name,
        &PatchParams::apply("upnp-controller"),
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(())
}

#[allow(dead_code)]
pub async fn remove_node_annotation(client: &Client, node_name: &str) -> Result<()> {
    let api: Api<Node> = Api::all(client.clone());
    let patch = json!({
        "metadata": {
            "annotations": {
                ANNOTATION_KEY: null
            }
        }
    });
    api.patch(
        node_name,
        &PatchParams::apply("upnp-controller"),
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(())
}
