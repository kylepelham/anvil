#pragma once
#include <QObject>

class Counter : public QObject {
    Q_OBJECT
    Q_PROPERTY(int value READ value NOTIFY valueChanged)

public:
    int value() const { return count; }

public slots:
    void increment();

signals:
    void valueChanged(int value);

private:
    int count = 0;
};
