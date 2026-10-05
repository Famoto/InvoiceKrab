//! The transformation contract of a spoke: what it reads into the hub, what it
//! needs to write, and what it declares it may lose.
//!
//! A transform `source → target` is fully determined by two contracts, because
//! the engine is hub-and-spoke: the source reads its canonical keys into the hub,
//! the target writes the hub back out. So no document is needed to know how a
//! pair behaves — [`crate::analysis`] compares the two contracts.
//!
//! Every spoke's contract is generated at build time from its compiled mapping
//! (`einvoice_dsl::contract`) into the registry as a `static`, reachable through
//! [`Spoke::contract`](crate::Spoke::contract). Everything in it is `'static`
//! data, so the types here are plain `const`-constructible structs.
//!
//! # Structure
//!
//! - [`TransformationContract`] — one spoke's contract.
//! - [`KeyContract`] — one canonical key the spoke maps (type, scope, codec, pin).
//! - [`RequiredRoute`] / [`Route`] — how each `required` node gets its write value.
//! - [`Collapse`] — a read-side operation that collapses repeated values.
//! - [`Selector`] — a `match` selector on a physical element.

/// One spoke's transformation contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransformationContract {
    /// The spoke's display name (its `Spoke::name`).
    pub spoke: &'static str,
    /// Every canonical key the spoke maps, sorted by label.
    pub keys: &'static [KeyContract],
    /// Every `required` node's write route, in node-id order.
    pub required: &'static [RequiredRoute],
    /// Read-side collapses (`multiple = "first" | "join"`): several source
    /// values become one hub value, so a document carrying several loses the
    /// rest. In node-id order.
    pub collapses: &'static [Collapse],
    /// `match` selectors: which occurrences of a shared physical element each
    /// logical node binds. In node-id order.
    pub selectors: &'static [Selector],
}

/// One canonical key a spoke maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyContract {
    /// The scope-qualified label (`InvoiceNumber`, `InvoiceLines/LineId`): the
    /// identity a key is compared by across spokes.
    pub label: &'static str,
    /// The canonical key itself.
    pub key: &'static str,
    /// The enclosing canonical collections, outermost first; empty at root.
    pub scope: &'static [&'static str],
    /// The semantic type (`identifier`, `decimal`, `date`, …, `collection`).
    pub ty: &'static str,
    /// The lexical codec the spoke reads and writes the key through, if any.
    pub codec: Option<&'static str>,
    /// The write-side constant the spoke emits instead of the hub value, if
    /// the key's node is also pinned (`constant` on a keyed node).
    pub pinned: Option<&'static str>,
}

/// How a `required` node obtains the value it writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// From the hub key with this label: the source must map it.
    Hub(&'static str),
    /// A fixed literal: always available.
    Constant(&'static str),
    /// Mirrors the hub key with this label (a `clone_of`): the source must
    /// map that key.
    Clone(&'static str),
}

impl Route {
    /// The hub label the route depends on, if it depends on one.
    pub fn needs(self) -> Option<&'static str> {
        match self {
            Route::Hub(label) | Route::Clone(label) => Some(label),
            Route::Constant(_) => None,
        }
    }
}

/// A `required` node and its write route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequiredRoute {
    /// The mapping node id.
    pub node: &'static str,
    /// Where its written value comes from.
    pub route: Route,
}

/// A read-side collapse of repeated source values into one hub value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collapse {
    /// The mapping node id.
    pub node: &'static str,
    /// The scope-qualified label of the key the node fills.
    pub label: &'static str,
    /// The `multiple` policy (`first` or `join`).
    pub policy: &'static str,
}

/// A `match` selector binding a logical node to some occurrences of a physical
/// element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selector {
    /// The logical mapping node id.
    pub node: &'static str,
    /// The physical element's XML local name.
    pub element: &'static str,
    /// `(child field path, expected text)` pairs; empty for the default bucket.
    pub selector: &'static [(&'static str, &'static str)],
    /// Whether the node keeps a single occurrence (a structural node) rather
    /// than every matching one (a collection). A single-valued node drops the
    /// surplus matches (`MATCH_MULTIPLE`).
    pub single: bool,
}

impl TransformationContract {
    /// The key contract with this label, if the spoke maps it.
    pub fn key(&self, label: &str) -> Option<&'static KeyContract> {
        self.keys.iter().find(|k| k.label == label)
    }

    /// Whether the spoke maps the key with this label.
    pub fn covers(&self, label: &str) -> bool {
        self.key(label).is_some()
    }

    /// The labels of every key the spoke maps, in label order.
    pub fn labels(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.keys.iter().map(|k| k.label)
    }

    /// The labels of the keys the spoke's `required` nodes need from the hub
    /// (`Route::Hub` and `Route::Clone` routes), de-duplicated and sorted.
    pub fn required_labels(&self) -> Vec<&'static str> {
        let mut labels: Vec<&'static str> = self
            .required
            .iter()
            .filter_map(|r| r.route.needs())
            .collect();
        labels.sort_unstable();
        labels.dedup();
        labels
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Spoke;

    #[test]
    fn test_every_spoke_contract_is_consistent() {
        for &spoke in Spoke::ALL {
            let c = spoke.contract();
            assert_eq!(c.spoke, spoke.name());
            assert!(!c.keys.is_empty(), "{} maps nothing", spoke.name());
            // Labels are sorted and unique: the identity keys are compared by.
            let labels: Vec<&str> = c.labels().collect();
            let mut sorted = labels.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(labels, sorted, "{} labels not sorted/unique", spoke.name());
            // A scope-qualified label is its scope chain plus its key.
            for k in c.keys {
                let expected = if k.scope.is_empty() {
                    k.key.to_string()
                } else {
                    format!("{}/{}", k.scope.join("/"), k.key)
                };
                assert_eq!(k.label, expected);
            }
            // Every hub route of a required node names a key the spoke maps.
            for r in c.required {
                if let Some(label) = r.route.needs() {
                    assert!(
                        c.covers(label),
                        "{}: {} needs {label}",
                        spoke.name(),
                        r.node
                    );
                }
            }
            for collapse in c.collapses {
                assert!(c.covers(collapse.label));
                assert!(matches!(collapse.policy, "first" | "join"));
            }
        }
    }

    #[test]
    fn test_ubl_contract_carries_the_known_routes_and_selectors() {
        let c = Spoke::UblInvoice.contract();
        let invoice_number = c.key("InvoiceNumber").expect("mapped");
        assert_eq!(invoice_number.ty, "identifier");
        assert!(invoice_number.scope.is_empty());
        let line_id = c.key("InvoiceLines/LineId").expect("mapped");
        assert_eq!(line_id.scope, ["InvoiceLines"]);
        assert_eq!(c.key("InvoiceLines").map(|k| k.ty), Some("collection"));
        assert!(
            c.required
                .iter()
                .any(|r| r.node == "Invoice.ID" && r.route == Route::Hub("InvoiceNumber"))
        );
        assert!(
            c.required
                .iter()
                .any(|r| r.node == "InvoiceLine" && r.route == Route::Hub("InvoiceLines"))
        );
        // The EN 16931 mandatory terms, including those inside collections.
        let required = c.required_labels();
        for label in [
            "InvoiceNumber",
            "IssueDate",
            "SellerName",
            "BuyerCountryCode",
            "PayableAmount",
            "VatBreakdown",
            "VatBreakdown/VatCategoryCode",
            "InvoiceLines",
            "InvoiceLines/QuantityUnitCode",
            "InvoiceLines/ItemNetPrice",
        ] {
            assert!(required.contains(&label), "{label} not in {required:?}");
        }
        // The specification identifier is pinned, so it is a constant route.
        assert!(
            c.required
                .iter()
                .all(|r| r.node != "Invoice.CustomizationID")
        );
        let object = c
            .selectors
            .iter()
            .find(|s| s.node == "Invoice.InvoicedObjectReference")
            .expect("BT-18 selector");
        assert_eq!(object.element, "AdditionalDocumentReference");
        assert_eq!(object.selector, [("document_type_code", "130")]);
        assert!(object.single);
        let supporting = c
            .selectors
            .iter()
            .find(|s| s.node == "Invoice.AdditionalDocumentReference")
            .expect("default bucket");
        assert!(supporting.selector.is_empty() && !supporting.single);
    }

    #[test]
    fn test_xrechnung_pins_the_specification_id_and_requires_the_process() {
        let c = Spoke::XrechnungInvoice.contract();
        let spec = c.key("SpecificationId").expect("mapped");
        assert_eq!(
            spec.pinned,
            Some("urn:cen.eu:en16931:2017#compliant#urn:xeinkauf.de:kosit:xrechnung_3.0"),
            "XRechnung writes its own CIUS identifier"
        );
        assert!(c.required_labels().contains(&"BusinessProcessType"));
        let peppol = Spoke::PeppolBisBilling.contract();
        assert_eq!(
            peppol.key("BusinessProcessType").and_then(|k| k.pinned),
            Some("urn:fdc:peppol.eu:2017:poacc:billing:01:1.0")
        );
        let facturx = Spoke::FacturxInvoice.contract();
        let date = facturx.key("IssueDate").expect("mapped");
        assert_eq!(date.codec, Some("cii-date-102"));
    }
}
