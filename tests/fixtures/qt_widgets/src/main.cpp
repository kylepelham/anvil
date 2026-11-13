#include "counter.h"
#include "mainwindow.h"
#include <QApplication>
#include <QFile>
#include <cstdio>

class Worker : public QObject {
    Q_OBJECT

signals:
    void done(const QString& text);
};

int main(int argc, char** argv)
{
#ifndef _WIN32
    // No display needed
    if (!qEnvironmentVariableIsSet("QT_QPA_PLATFORM"))
        qputenv("QT_QPA_PLATFORM", "offscreen");
#endif
    QApplication app(argc, argv);

    // Signals and slots of a header's class
    Counter counter;
    int seen = 0;
    QObject::connect(&counter, &Counter::valueChanged, [&](int value) { seen = value; });
    counter.increment();
    counter.increment();
    std::printf("counter: %d %s\n", seen, counter.metaObject()->className());

    // The form
    MainWindow window;
    std::printf("form: %s\n", qPrintable(window.labelText()));

    // The resource
    QFile file(":/data/hello.txt");
    if (!file.open(QIODevice::ReadOnly))
        return 1;
    std::printf("resource: %s", file.readAll().constData());

    // A class in this file
    Worker worker;
    QString got;
    QObject::connect(&worker, &Worker::done, [&](const QString& text) { got = text; });
    emit worker.done("worker");
    std::printf("source moc: %s\n", qPrintable(got));
    return 0;
}

#include "main.moc"
