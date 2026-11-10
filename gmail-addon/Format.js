/**
 * Summaries of the records Mailsift extracted from a message, for the
 * Gmail sidebar. Field choices follow the record lists in mailsift's web
 * UI (`src/web.rs`).
 */

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const WEEKDAYS = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];

/**
 * The first non-empty string among `keys` of `obj`.
 *
 * @param {*} obj
 * @param {Array<string>} keys
 * @return {string|null}
 */
function pickStr(obj, keys) {
  if (!obj || typeof obj !== 'object') {
    return null;
  }
  for (const key of keys) {
    const value = obj[key];
    if (typeof value === 'string' && value.trim()) {
      return value.trim();
    }
    if (typeof value === 'number') {
      return String(value);
    }
  }
  return null;
}

/**
 * Name of a schema.org node that may be a bare string or an object with
 * a `name` (or, for airlines, an `iataCode`).
 *
 * @param {*} value
 * @return {string|null}
 */
function named(value) {
  if (typeof value === 'string') {
    return value.trim() || null;
  }
  return pickStr(value, ['name', 'iataCode']);
}

/**
 * Format a date or date-time string as written in the record, e.g.
 * `Sun 20 Dec 2026, 17:13`. The time is shown as written (the local
 * time of the trip), not converted to the viewer's time zone. Strings
 * that don't start with an ISO date are returned unchanged.
 *
 * @param {string} raw
 * @param {boolean} withTime
 * @return {string}
 */
function formatWhen(raw, withTime) {
  const m = /^(\d{4})-(\d{2})-(\d{2})(?:[T ](\d{2}):(\d{2}))?/.exec(raw);
  if (!m) {
    return raw;
  }
  const [, y, mo, d, hh, mm] = m;
  const weekday = WEEKDAYS[new Date(Date.UTC(+y, +mo - 1, +d)).getUTCDay()];
  const date = `${weekday} ${+d} ${MONTHS[+mo - 1]} ${y}`;
  return withTime && hh !== undefined ? `${date}, ${hh}:${mm}` : date;
}

/**
 * Format a `{ price, priceCurrency }` node, e.g. `81.2 GBP`.
 *
 * @param {*} spec
 * @return {string|null}
 */
function formatPrice(spec) {
  const price = pickStr(spec, ['price']);
  if (price === null) {
    return null;
  }
  const currency = pickStr(spec, ['priceCurrency']);
  return currency ? `${price} ${currency}` : price;
}

/**
 * Turn a schema.org enumeration value such as `OnItsWay` or
 * `https://schema.org/InTransit` into `On its way` / `In transit`.
 *
 * @param {string} value
 * @return {string}
 */
function humanizeEnum(value) {
  const words = value
    .replace(/^.*\//, '')
    .replace(/([a-z])([A-Z])/g, '$1 $2')
    .toLowerCase();
  return words.charAt(0).toUpperCase() + words.slice(1);
}

function joinParts(parts) {
  return parts.filter(Boolean).join(' · ');
}

function searchPath(listPath, query) {
  return query ? `${listPath}?q=${encodeURIComponent(query)}` : listPath;
}

const RESERVATION_KINDS = {
  Flight: { label: 'Flight', icon: 'AIRPLANE', timed: true },
  TrainTrip: { label: 'Train', icon: 'TRAIN', timed: true },
  BusTrip: { label: 'Coach', icon: 'BUS', timed: true },
  BoatTrip: { label: 'Ferry', icon: 'MAP_PIN', timed: true },
  LodgingBusiness: { label: 'Hotel', icon: 'HOTEL', timed: false },
  Hotel: { label: 'Hotel', icon: 'HOTEL', timed: false },
  FoodEstablishment: { label: 'Restaurant', icon: 'RESTAURANT_ICON', timed: true },
  Restaurant: { label: 'Restaurant', icon: 'RESTAURANT_ICON', timed: true },
  Event: { label: 'Event', icon: 'TICKET', timed: true },
};

/**
 * Who a reservation is with: the airline, then a provider (top-level or
 * under `reservationFor`), then the broker, then the name of the thing
 * reserved. Same precedence as `reservation_provider` in mailsift.
 *
 * @param {Object} data
 * @return {string|null}
 */
function reservationProvider(data) {
  const forNode = data.reservationFor || {};
  return (
    named(forNode.airline) ||
    named(data.provider) ||
    named(forNode.provider) ||
    named(data.broker) ||
    named(forNode.name)
  );
}

/**
 * The route: airport codes for flights, station or stop names otherwise.
 *
 * @param {Object} forNode `reservationFor`
 * @return {string|null}
 */
function reservationRoute(forNode) {
  let from;
  let to;
  if (forNode['@type'] === 'Flight') {
    from = pickStr(forNode.departureAirport, ['iataCode', 'name']);
    to = pickStr(forNode.arrivalAirport, ['iataCode', 'name']);
  } else {
    from = pickStr(forNode.departureStation || forNode.departureBusStop || forNode.departureBoatTerminal, ['name']);
    to = pickStr(forNode.arrivalStation || forNode.arrivalBusStop || forNode.arrivalBoatTerminal, ['name']);
  }
  if (from && to) {
    return `${from} → ${to}`;
  }
  return from || to;
}

function summarizeReservation(data) {
  const forNode = data.reservationFor && typeof data.reservationFor === 'object' ? data.reservationFor : {};
  const kind = RESERVATION_KINDS[forNode['@type']] || {
    label: 'Reservation',
    icon: 'CONFIRMATION_NUMBER_ICON',
    timed: true,
  };
  const provider = reservationProvider(data);
  const route = reservationRoute(forNode);
  const ref = pickStr(data, ['reservationNumber', 'reservationId', 'identifier']);
  const status = pickStr(data, ['reservationStatus']);
  const cancelled = Boolean(status && /cancel/i.test(status));

  let when;
  const checkin = pickStr(data, ['checkinTime']);
  if (checkin) {
    const checkout = pickStr(data, ['checkoutTime']);
    when = checkout
      ? `${formatWhen(checkin, false)} – ${formatWhen(checkout, false)}`
      : formatWhen(checkin, false);
  } else {
    const start =
      pickStr(forNode, ['departureTime', 'startDate', 'doorTime']) ||
      pickStr(data, ['startTime']);
    when = start ? formatWhen(start, kind.timed) : null;
  }

  const title = route || provider || kind.label;
  return {
    label: cancelled ? `${kind.label} (cancelled)` : kind.label,
    icon: kind.icon,
    title,
    subtitle: joinParts([route && provider, when, ref]),
    path: searchPath('/reservations', ref),
  };
}

/**
 * Summarise one record from the `records` of a `POST /api/messages`
 * response.
 *
 * @param {string} kind `reservation`, `bill`, `parcel`, `receipt`,
 *   `ticket` or `subscription`
 * @param {Object} data the record as the extractor wrote it
 * @return {{label: string, icon: string, title: string, subtitle: string,
 *   path: string}} `icon` names a `CardService.Icon`; `path` is a page
 *   on the Mailsift server
 */
function summarizeRecord(kind, data) {
  switch (kind) {
    case 'reservation':
      return summarizeReservation(data);
    case 'bill': {
      const payee = named(data.payee) || pickStr(data, ['accountName']) || 'Bill';
      const invoice = pickStr(data, ['invoiceNumber', 'identifier']);
      const due = pickStr(data, ['dueDate', 'paymentDueDate']);
      return {
        label: 'Bill',
        icon: 'DOLLAR',
        title: joinParts([payee, formatPrice(data.totalPaymentDue)]),
        subtitle: joinParts([invoice, due && `due ${formatWhen(due, false)}`]),
        path: searchPath('/bills', invoice),
      };
    }
    case 'parcel': {
      const tracking = pickStr(data, ['trackingNumber', 'identifier']);
      const carrier = named(data.provider) || named(data.carrier);
      const status = pickStr(data, ['deliveryStatus']);
      return {
        label: 'Parcel',
        icon: 'MAP_PIN',
        title: [carrier, tracking].filter(Boolean).join(' ') || 'Parcel',
        subtitle: status ? humanizeEnum(status) : '',
        path: searchPath('/parcels', tracking),
      };
    }
    case 'receipt': {
      const merchant = named(data.merchant) || named(data.seller) || 'Receipt';
      const order = pickStr(data, ['orderNumber', 'identifier']);
      const date = pickStr(data, ['orderDate', 'date']);
      return {
        label: 'Receipt',
        icon: 'SHOPPING_CART',
        title: joinParts([merchant, formatPrice(data.priceSpecification)]),
        subtitle: joinParts([order, date && formatWhen(date, false)]),
        path: searchPath('/receipts', order),
      };
    }
    case 'subscription': {
      const name = pickStr(data, ['name']) || named(data.provider) || 'Subscription';
      const renewal = pickStr(data, ['renewalDate', 'nextPaymentDate']);
      return {
        label: 'Subscription',
        icon: 'MEMBERSHIP',
        title: joinParts([name, formatPrice(data)]),
        subtitle: renewal ? `renews ${formatWhen(renewal, false)}` : '',
        path: searchPath('/subscriptions', name),
      };
    }
    case 'ticket': {
      const event = data.reservationFor || data.event;
      return {
        label: 'Ticket',
        icon: 'TICKET',
        title: named(event) || pickStr(data, ['name']) || 'Ticket',
        subtitle: joinParts([
          pickStr(event, ['startDate']) && formatWhen(pickStr(event, ['startDate']), true),
          pickStr(data, ['ticketNumber', 'reservationNumber']),
        ]),
        path: '/tickets',
      };
    }
    default:
      throw new Error(`Unknown record kind ${kind}`);
  }
}

if (typeof module !== 'undefined' && module.exports) {
  module.exports = { summarizeRecord, formatWhen };
}
