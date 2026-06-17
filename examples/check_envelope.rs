fn main() {
    let val = stellar_xdr::curr::EnvelopeType::Auth as i32;
    println!("EnvelopeType::Auth as i32 = {}", val);
    let xdr_bytes = stellar_xdr::curr::EnvelopeType::Auth.to_xdr(stellar_xdr::curr::Limits::none()).unwrap();
    println!("XDR bytes: {:02x?}", xdr_bytes);
    println!("Raw be_bytes: {:02x?}", (val as i32).to_be_bytes());
}
