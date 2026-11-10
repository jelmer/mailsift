const test = require('node:test');
const assert = require('node:assert/strict');
const { summarizeRecord, formatWhen } = require('../Format.js');

test('summarizeRecord describes a train reservation by route and departure', () => {
  const summary = summarizeRecord('reservation', {
    '@type': 'TrainReservation',
    reservationNumber: 'eurostar-ABC123-return',
    reservationFor: {
      '@type': 'TrainTrip',
      provider: { '@type': 'Organization', name: 'Eurostar' },
      departureStation: { '@type': 'TrainStation', name: 'Paris Nord' },
      arrivalStation: { '@type': 'TrainStation', name: 'London St Pancras' },
      departureTime: '2026-12-20T17:13:00',
      arrivalTime: '2026-12-20T18:30:00',
    },
  });

  assert.deepEqual(summary, {
    label: 'Train',
    icon: 'TRAIN',
    title: 'Paris Nord → London St Pancras',
    subtitle: 'Eurostar · Sun 20 Dec 2026, 17:13 · eurostar-ABC123-return',
    path: '/reservations?q=eurostar-ABC123-return',
  });
});

test('summarizeRecord describes a flight by airports and airline', () => {
  const summary = summarizeRecord('reservation', {
    reservationNumber: 'XYZ789',
    reservationStatus: 'https://schema.org/ReservationCancelled',
    reservationFor: {
      '@type': 'Flight',
      airline: { iataCode: 'KL', name: 'KLM' },
      flightNumber: '1004',
      departureAirport: { iataCode: 'LHR', name: 'Heathrow' },
      arrivalAirport: { iataCode: 'AMS' },
      departureTime: '2026-11-30T07:05:00+00:00',
    },
  });

  assert.deepEqual(summary, {
    label: 'Flight (cancelled)',
    icon: 'AIRPLANE',
    title: 'LHR → AMS',
    subtitle: 'KLM · Mon 30 Nov 2026, 07:05 · XYZ789',
    path: '/reservations?q=XYZ789',
  });
});

test('summarizeRecord describes a hotel stay by name and dates', () => {
  const summary = summarizeRecord('reservation', {
    reservationFor: { '@type': 'LodgingBusiness', name: 'Hotel Example' },
    checkinTime: '2026-12-01',
    checkoutTime: '2026-12-03',
    reservationNumber: 'H-1',
  });

  assert.deepEqual(summary, {
    label: 'Hotel',
    icon: 'HOTEL',
    title: 'Hotel Example',
    subtitle: 'Tue 1 Dec 2026 – Thu 3 Dec 2026 · H-1',
    path: '/reservations?q=H-1',
  });
});

test('summarizeRecord describes bills, parcels, receipts and subscriptions', () => {
  assert.deepEqual(
    summarizeRecord('bill', {
      payee: 'Example Energy',
      invoiceNumber: 'INV-42',
      totalPaymentDue: { price: 81.2, priceCurrency: 'GBP' },
      paymentDueDate: '2026-12-15',
    }),
    {
      label: 'Bill',
      icon: 'DOLLAR',
      title: 'Example Energy · 81.2 GBP',
      subtitle: 'INV-42 · due Tue 15 Dec 2026',
      path: '/bills?q=INV-42',
    }
  );
  assert.deepEqual(
    summarizeRecord('parcel', {
      trackingNumber: 'DHL999',
      deliveryStatus: 'OnItsWay',
      provider: { name: 'DHL' },
    }),
    {
      label: 'Parcel',
      icon: 'MAP_PIN',
      title: 'DHL DHL999',
      subtitle: 'On its way',
      path: '/parcels?q=DHL999',
    }
  );
  assert.deepEqual(
    summarizeRecord('receipt', {
      merchant: { name: 'Example Shop' },
      orderNumber: 'O-7',
      orderDate: '2026-11-08',
      priceSpecification: { price: '19.99', priceCurrency: 'EUR' },
    }),
    {
      label: 'Receipt',
      icon: 'SHOPPING_CART',
      title: 'Example Shop · 19.99 EUR',
      subtitle: 'O-7 · Sun 8 Nov 2026',
      path: '/receipts?q=O-7',
    }
  );
  assert.deepEqual(
    summarizeRecord('subscription', {
      name: 'Example Music',
      price: 9.99,
      priceCurrency: 'EUR',
      renewalDate: '2026-12-01',
    }),
    {
      label: 'Subscription',
      icon: 'MEMBERSHIP',
      title: 'Example Music · 9.99 EUR',
      subtitle: 'renews Tue 1 Dec 2026',
      path: '/subscriptions?q=Example%20Music',
    }
  );
});

test('summarizeRecord never shows a nested object as text', () => {
  const summary = summarizeRecord('reservation', {
    provider: { '@type': 'Organization' },
    reservationFor: { '@type': 'Thing', name: { nested: true } },
  });

  assert.equal(summary.title, 'Reservation');
  assert.equal(JSON.stringify(summary).includes('[object Object]'), false);
});

test('formatWhen keeps the local time written in the record', () => {
  assert.equal(formatWhen('2026-12-20T17:13:00+01:00', true), 'Sun 20 Dec 2026, 17:13');
  assert.equal(formatWhen('2026-12-20T17:13:00', false), 'Sun 20 Dec 2026');
  assert.equal(formatWhen('next week', true), 'next week');
});
